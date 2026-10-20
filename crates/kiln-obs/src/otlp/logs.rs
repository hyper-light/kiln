//! OTLP logs into records (`docs/observe.md` §4.6, §8): an `ExportLogsServiceRequest`
//! (opentelemetry-proto `collector/logs/v1`, v1.5.0) read in place.
//!
//! - The time is the record's (`time_unix_nano`), else when it was observed, else when it was
//!   received.
//! - The severity is its number (OpenTelemetry's 1–24), else what its text names (`ERROR`,
//!   `warning`, ...), else unspecified.
//! - `service` and `host` come from the resource's `service.name` and `host.name` (Datadog's
//!   unified service tags, as metrics take them), and `source` from the instrumentation scope's
//!   name, else "otlp".
//! - The message is the body: a string as it is, anything else as compact JSON.
//! - The attributes are the record's over its resource's (but the two named above), each value as
//!   text (of a span's attribute's bound: a log's holds a stack), at most `MAX_TAGS`; the event's
//!   name, if any, is the attribute `event.name`; `env` and `version` the resource's
//!   `deployment.environment.name` (or `deployment.environment`) and `service.version` where the
//!   record says neither (Datadog's unified service tags, as spans take them).
//! - A trace id of 16 bytes and a span id of 8 are kept; others (empty: none) are not.

use super::metrics::{OtlpError, any_value_push, any_value_text_within, key_value_within};
use super::pb::Fields;
use super::traces::{Attr, MAX_VALUE_BYTES, SEGMENT_BYTES, SPARES, Val, attrs_for, cut, find, resolve};
use crate::batch::{MAX_NAME_BYTES, MAX_TAGS};
use crate::logs::record::{LogPart, MAX_MESSAGE_BYTES, Record, put_fields, severities};

/// What a request's records came to.
#[derive(Debug, Default)]
pub struct DecodedLogs {
    pub records: Vec<Record>,
    /// Attributes past `MAX_TAGS` on a record, dropped in key order.
    pub dropped_attrs: u64,
}

/// The records of an `ExportLogsServiceRequest`; `now_ns` stands for a record that says no time.
pub fn decode(body: &[u8], now_ns: i64) -> Result<DecodedLogs, OtlpError> {
    let mut out = DecodedLogs::default();
    for f in Fields::new(body) {
        if let (1, w) = f? {
            resource_logs(w.bytes().unwrap_or(&[]), now_ns, &mut out)?;
        }
    }
    Ok(out)
}

fn resource_logs(msg: &[u8], now_ns: i64, out: &mut DecodedLogs) -> Result<(), OtlpError> {
    let mut resource: Vec<(String, String)> = Vec::new();
    let mut scopes: Vec<&[u8]> = Vec::new();
    for f in Fields::new(msg) {
        match f? {
            (1, w) => {
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    if let (1, kv) = f?
                        && let Some(t) = key_value_within(kv.bytes().unwrap_or(&[]), MAX_VALUE_BYTES)?
                    {
                        resource.push(t);
                    }
                }
            }
            (2, w) => scopes.push(w.bytes().unwrap_or(&[])),
            _ => {}
        }
    }
    let service = resource.iter().find(|(k, _)| k == "service.name").map(|(_, v)| v.clone()).unwrap_or_default();
    let host = resource.iter().find(|(k, _)| k == "host.name").map(|(_, v)| v.clone()).unwrap_or_default();
    resource.retain(|(k, _)| k != "service.name" && k != "host.name");
    for scope in scopes {
        let mut source = String::new();
        let mut records: Vec<&[u8]> = Vec::new();
        for f in Fields::new(scope) {
            match f? {
                (1, w) => {
                    for f in Fields::new(w.bytes().unwrap_or(&[])) {
                        if let (1, n) = f? {
                            source = n.str().unwrap_or("").to_owned();
                        }
                    }
                }
                (2, w) => records.push(w.bytes().unwrap_or(&[])),
                _ => {}
            }
        }
        if source.is_empty() {
            source = "otlp".into();
        }
        for r in records {
            let rec = record(r, &service, &host, &source, &resource, now_ns, out)?;
            out.records.push(rec);
        }
    }
    Ok(())
}

fn record(msg: &[u8], service: &str, host: &str, source: &str, resource: &[(String, String)], now_ns: i64, out: &mut DecodedLogs) -> Result<Record, OtlpError> {
    let (mut time, mut observed, mut number, mut text) = (0u64, 0u64, 0u64, String::new());
    let mut body = String::new();
    let mut attrs: Vec<(String, String)> = Vec::new();
    let (mut trace_id, mut span_id) = (None, None);
    let mut event = None;
    for f in Fields::new(msg) {
        match f? {
            (1, w) => time = w.u64().unwrap_or(0),
            (11, w) => observed = w.u64().unwrap_or(0),
            (2, w) => number = w.u64().unwrap_or(0),
            (3, w) => text = w.str().unwrap_or("").to_owned(),
            (5, w) => body = any_value_text_within(w.bytes().unwrap_or(&[]), MAX_MESSAGE_BYTES)?,
            (6, w) => {
                // A stack read whole (its row keeps its ends), any other value to a span's bound.
                let kv = w.bytes().unwrap_or(&[]);
                let stack = Fields::new(kv).any(|f| matches!(f, Ok((1, k)) if k.str().is_some_and(|k| crate::errors::STACK_KEYS.contains(&k))));
                if let Some(t) = key_value_within(kv, if stack { MAX_MESSAGE_BYTES } else { MAX_VALUE_BYTES })? {
                    attrs.push(t);
                }
            }
            (9, w) => trace_id = w.bytes().and_then(|b| <[u8; 16]>::try_from(b).ok()),
            (10, w) => span_id = w.bytes().and_then(|b| <[u8; 8]>::try_from(b).ok()),
            (12, w) => event = w.str().filter(|e| !e.is_empty()).map(str::to_owned),
            _ => {}
        }
    }
    for (k, v) in resource {
        if !attrs.iter().any(|(ak, _)| ak == k) {
            attrs.push((k.clone(), v.clone()));
        }
    }
    // The unified service tags where the record says none.
    for (tag, from) in [("env", &["deployment.environment.name", "deployment.environment"][..]), ("version", &["service.version"][..])] {
        if !attrs.iter().any(|(k, _)| k == tag)
            && let Some(v) = from.iter().find_map(|f| attrs.iter().find(|(k, v)| k == f && !v.is_empty()).map(|(_, v)| v.clone()))
        {
            attrs.push((tag.to_owned(), v));
        }
    }
    if let Some(e) = event {
        attrs.push(("event.name".into(), e));
    }
    if attrs.len() > MAX_TAGS {
        attrs.sort_unstable();
        out.dropped_attrs += (attrs.len() - MAX_TAGS) as u64;
        attrs.truncate(MAX_TAGS);
    }
    let severity = match u8::try_from(number) {
        Ok(n @ 1..=24) => n,
        _ => severities(text.trim()).map_or(0, |r| *r.start()),
    };
    let ns = if time != 0 { time } else { observed };
    Ok(Record {
        ts: if ns == 0 { now_ns } else { i64::try_from(ns).unwrap_or(i64::MAX) },
        severity,
        service: service.to_owned(),
        host: host.to_owned(),
        source: source.to_owned(),
        message: body,
        attrs,
        trace_id: trace_id.filter(|t| t.iter().any(|&b| b != 0)),
        span_id: span_id.filter(|s| s.iter().any(|&b| b != 0)),
    })
}

/// Decodes requests' log records straight into rows (the log rows' encoding, as
/// `super::traces::Decoder` writes spans'), its room kept from one request to the next: no record
/// is made of one on the way, and a decoder that has decoded a request allocates nothing for the
/// next but rows. A record a pipeline reads (as the caller's `reads` says, asked of a [`LogView`])
/// is made the record [`decode`] makes instead, in its place among the rows. Each row is the row
/// [`decode`]'s record writes ([`crate::logs::record::put_row`]), its error's issue on it as
/// [`crate::errors::put_log_issue`] puts one: a test holds them alike.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Values written (the resource's first, then a record's).
    text: String,
    resource: Vec<Attr<'static>>,
    attrs: Vec<Attr<'static>>,
    /// Row buffers handed back ([`Decoder::give_rows`]).
    spare: Vec<Vec<u8>>,
}

/// What a request's records came to, in their order: runs of records (those a pipeline reads) and
/// segments of rows (each at most [`SEGMENT_BYTES`] but its last row).
#[derive(Debug, Default)]
pub struct Decoded {
    pub parts: Vec<LogPart>,
    /// Records, whichever form.
    pub records: u64,
    /// Attributes past `MAX_TAGS` on a record, dropped in key order.
    pub dropped_attrs: u64,
}

/// A record as [`Decoder`] reads it, borrowed: what a pipeline's filter is asked of.
pub struct LogView<'a> {
    pub message: &'a str,
    pub service: &'a str,
    pub host: &'a str,
    pub source: &'a str,
    pub severity: u8,
    attrs: &'a [Attr<'a>],
    text: &'a str,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
}

impl LogView<'_> {
    /// Its attributes, in its record's order.
    pub fn attrs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.attrs.iter().map(|a| (a.key, resolve(a.val, self.text)))
    }
}

impl crate::logs::search::Fields for LogView<'_> {
    fn message(&self) -> &str {
        self.message
    }
    fn service(&self) -> &str {
        self.service
    }
    fn host(&self) -> &str {
        self.host
    }
    fn source(&self) -> &str {
        self.source
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
}

/// An `AnyValue` of at most `max` bytes as [`any_value_text_within`] reads it: a string the
/// request's own, anything else written into `text`.
fn value_within<'a>(v: &'a [u8], text: &mut String, max: usize) -> Result<Val<'a>, OtlpError> {
    for f in Fields::new(v) {
        match f? {
            (1, w) => return Ok(Val::Str(cut(w.str().unwrap_or(""), max))),
            (2..=7, _) => {
                let start = text.len();
                any_value_push(v, max, text)?;
                return Ok(Val::Text(start, text.len()));
            }
            _ => {}
        }
    }
    Ok(Val::Str(""))
}

/// A `KeyValue` as [`key_value_within`] reads it, borrowed: its key cut at a name's length, its
/// value at `max` bytes (each value field read, the last kept); none without a key.
fn attr_within<'a>(msg: &'a [u8], text: &mut String, max: usize) -> Result<Option<Attr<'a>>, OtlpError> {
    let (mut key, mut value) = (None, None);
    for f in Fields::new(msg) {
        match f? {
            (1, w) => key = w.str(),
            (2, w) => value = Some(value_within(w.bytes().unwrap_or(&[]), text, max)?),
            _ => {}
        }
    }
    Ok(key.filter(|k| !k.is_empty()).map(|k| Attr { key: cut(k, MAX_NAME_BYTES), val: value.unwrap_or(Val::Str("")) }))
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer to write rows into (one a write held and handed back, emptied); past [`SPARES`]
    /// kept, it goes.
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

    /// The records of an `ExportLogsServiceRequest` (or a `LogsData`: the same message) into
    /// `out`, a record `reads` says a pipeline reads made a record; `now_ns` stands for a record
    /// that says no time.
    pub fn decode(&mut self, body: &[u8], now_ns: i64, reads: &mut dyn FnMut(&LogView<'_>) -> bool, out: &mut Decoded) -> Result<(), OtlpError> {
        let mut cx =
            Cx { text: std::mem::take(&mut self.text), resource: attrs_for(std::mem::take(&mut self.resource)), attrs: attrs_for(std::mem::take(&mut self.attrs)), spare: &mut self.spare, seg: None };
        let r = (|| {
            for f in Fields::new(body) {
                if let (1, w) = f? {
                    cx.resource_logs(w.bytes().unwrap_or(&[]), now_ns, reads, out)?;
                }
            }
            Ok(())
        })();
        cx.seal(out);
        let Cx { mut text, resource, attrs, .. } = cx;
        text.clear();
        self.text = text;
        self.resource = attrs_for(resource);
        self.attrs = attrs_for(attrs);
        r
    }
}

/// The segment being written: its rows, how many, the last one's time.
struct Seg {
    raw: Vec<u8>,
    rows: u32,
    prev: i64,
}

/// One request's decoding: the decoder's room, lent for its borrows.
struct Cx<'a, 'd> {
    text: String,
    resource: Vec<Attr<'a>>,
    attrs: Vec<Attr<'a>>,
    spare: &'d mut Vec<Vec<u8>>,
    seg: Option<Seg>,
}

impl<'a> Cx<'a, '_> {
    /// The segment being written, ended: a part of `out` if it holds a row.
    fn seal(&mut self, out: &mut Decoded) {
        if let Some(s) = self.seg.take() {
            if s.rows > 0 {
                out.parts.push(LogPart::Rows { raw: s.raw, rows: s.rows, last_ts: s.prev });
            } else if self.spare.len() < SPARES {
                self.spare.push(s.raw);
            }
        }
    }

    fn resource_logs(&mut self, msg: &'a [u8], now_ns: i64, reads: &mut dyn FnMut(&LogView<'_>) -> bool, out: &mut Decoded) -> Result<(), OtlpError> {
        // Values the resource's records wrote are in their rows (or records) by now.
        self.text.clear();
        self.resource.clear();
        for f in Fields::new(msg) {
            if let (1, w) = f? {
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    if let (1, kv) = f?
                        && let Some(a) = attr_within(kv.bytes().unwrap_or(&[]), &mut self.text, MAX_VALUE_BYTES)?
                    {
                        self.resource.push(a);
                    }
                }
            }
        }
        let service = find(&self.resource, "service.name");
        let host = find(&self.resource, "host.name");
        self.resource.retain(|a| a.key != "service.name" && a.key != "host.name");
        for f in Fields::new(msg) {
            if let (2, w) = f? {
                let scope = w.bytes().unwrap_or(&[]);
                // Its name first (the last one given), as `decode` reads it before its records.
                let mut source = "";
                for f in Fields::new(scope) {
                    if let (1, w) = f? {
                        for f in Fields::new(w.bytes().unwrap_or(&[])) {
                            if let (1, n) = f? {
                                source = n.str().unwrap_or("");
                            }
                        }
                    }
                }
                if source.is_empty() {
                    source = "otlp";
                }
                for f in Fields::new(scope) {
                    if let (2, w) = f? {
                        self.record(w.bytes().unwrap_or(&[]), [service, host], source, now_ns, reads, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record(&mut self, msg: &'a [u8], names: [Option<Val<'a>>; 2], source: &str, now_ns: i64, reads: &mut dyn FnMut(&LogView<'_>) -> bool, out: &mut Decoded) -> Result<(), OtlpError> {
        let mark = self.text.len();
        self.attrs.clear();
        let (mut time, mut observed, mut number, mut said) = (0u64, 0u64, 0u64, "");
        let mut body = Val::Str("");
        let (mut trace_id, mut span_id, mut event) = (None, None, None);
        for f in Fields::new(msg) {
            match f? {
                (1, w) => time = w.u64().unwrap_or(0),
                (11, w) => observed = w.u64().unwrap_or(0),
                (2, w) => number = w.u64().unwrap_or(0),
                (3, w) => said = w.str().unwrap_or(""),
                (5, w) => body = value_within(w.bytes().unwrap_or(&[]), &mut self.text, MAX_MESSAGE_BYTES)?,
                (6, w) => {
                    // A stack read whole (its row keeps its ends), any other value to a span's bound.
                    let kv = w.bytes().unwrap_or(&[]);
                    let stack = Fields::new(kv).any(|f| matches!(f, Ok((1, k)) if k.str().is_some_and(|k| crate::errors::STACK_KEYS.contains(&k))));
                    if let Some(a) = attr_within(kv, &mut self.text, if stack { MAX_MESSAGE_BYTES } else { MAX_VALUE_BYTES })? {
                        self.attrs.push(a);
                    }
                }
                (9, w) => trace_id = w.bytes().and_then(|b| <[u8; 16]>::try_from(b).ok()),
                (10, w) => span_id = w.bytes().and_then(|b| <[u8; 8]>::try_from(b).ok()),
                (12, w) => event = w.str().filter(|e| !e.is_empty()),
                _ => {}
            }
        }
        for u in &self.resource {
            if !self.attrs.iter().any(|a| a.key == u.key) {
                self.attrs.push(*u);
            }
        }
        // The unified service tags where the record says none.
        for (tag, from) in [("env", &["deployment.environment.name", "deployment.environment"][..]), ("version", &["service.version"][..])] {
            if !self.attrs.iter().any(|a| a.key == tag)
                && let Some(v) = from.iter().find_map(|f| self.attrs.iter().find(|a| a.key == *f && !resolve(a.val, &self.text).is_empty()).map(|a| a.val))
            {
                self.attrs.push(Attr { key: tag, val: v });
            }
        }
        if let Some(e) = event {
            self.attrs.push(Attr { key: "event.name", val: Val::Str(e) });
        }
        if self.attrs.len() > MAX_TAGS {
            let text: &str = &self.text;
            self.attrs.sort_unstable_by(|a, b| (a.key, resolve(a.val, text)).cmp(&(b.key, resolve(b.val, text))));
            out.dropped_attrs += (self.attrs.len() - MAX_TAGS) as u64;
            self.attrs.truncate(MAX_TAGS);
        }
        let severity = match u8::try_from(number) {
            Ok(n @ 1..=24) => n,
            _ => severities(said.trim()).map_or(0, |r| *r.start()),
        };
        let ns = if time != 0 { time } else { observed };
        let ts = if ns == 0 { now_ns } else { i64::try_from(ns).unwrap_or(i64::MAX) };
        let (trace_id, span_id) = (trace_id.filter(|t| t.iter().any(|&b| b != 0)), span_id.filter(|s| s.iter().any(|&b| b != 0)));
        let read = {
            let text: &str = &self.text;
            let [service, host] = names.map(|n| n.map_or("", |v| resolve(v, text)));
            let view = LogView { message: resolve(body, text), service, host, source, severity, attrs: &self.attrs, text, trace_id, span_id };
            reads(&view)
        };
        out.records += 1;
        if read {
            let text: &str = &self.text;
            let [service, host] = names.map(|n| n.map_or("", |v| resolve(v, text)).to_owned());
            let record = Record {
                ts,
                severity,
                service,
                host,
                source: source.to_owned(),
                message: resolve(body, text).to_owned(),
                attrs: self.attrs.iter().map(|a| (a.key.to_owned(), resolve(a.val, text).to_owned())).collect(),
                trace_id,
                span_id,
            };
            self.seal(out);
            match out.parts.last_mut() {
                Some(LogPart::Records(run)) => run.push(record),
                _ => out.parts.push(LogPart::Records(vec![record])),
            }
            self.text.truncate(mark);
            return Ok(());
        }
        // An error's issue, as `crate::errors::put_log_issue` gives a record one: one it came with
        // left out, its own put last.
        if severity >= 17 {
            self.attrs.retain(|a| a.key != "issue.id");
            let id = {
                let text: &str = &self.text;
                let service = names[0].map_or("", |v| resolve(v, text));
                crate::errors::log_issue(service, severity, &crate::errors::Error::of(self.attrs.iter().map(|a| (a.key, resolve(a.val, text)))))
            };
            if let Some(id) = id {
                use std::fmt::Write as _;
                let at = self.text.len();
                let _ = write!(self.text, "{id:016x}");
                self.attrs.push(Attr { key: "issue.id", val: Val::Text(at, self.text.len()) });
            }
        }
        let seg = match &mut self.seg {
            Some(s) if s.raw.len() < SEGMENT_BYTES => s,
            slot => {
                if let Some(full) = slot.take() {
                    out.parts.push(LogPart::Rows { raw: full.raw, rows: full.rows, last_ts: full.prev });
                }
                slot.insert(Seg { raw: self.spare.pop().unwrap_or_default(), rows: 0, prev: 0 })
            }
        };
        let text: &str = &self.text;
        let [service, host] = names.map(|n| n.map_or("", |v| resolve(v, text)));
        put_fields(
            &mut seg.raw,
            ts,
            severity,
            [service, host, source],
            resolve(body, text),
            self.attrs.iter().map(|a| (a.key, resolve(a.val, text))),
            self.attrs.len(),
            trace_id,
            span_id,
            seg.prev,
            MAX_NAME_BYTES,
        );
        seg.prev = ts;
        seg.rows += 1;
        self.text.truncate(mark);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::pb::Writer;

    fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
        w.message(field, |m| {
            m.str(1, k);
            m.message(2, |a| a.str(1, v));
        });
    }

    /// Words keys and values are made of: the names the decoding reads, stacks and errors, cuts'
    /// lengths, characters of every width, nothing.
    fn word(rng: &mut crate::testkit::Rng) -> String {
        const WORDS: [&str; 22] = [
            "",
            "a",
            "b",
            "service.name",
            "host.name",
            "deployment.environment",
            "deployment.environment.name",
            "service.version",
            "env",
            "version",
            "event.name",
            "issue.id",
            "error.kind",
            "error.type",
            "error.message",
            "error.stack",
            "é漢🙂",
            "ERROR",
            "warn",
            "Info",
            "FATAL",
            "x",
        ];
        match rng.below(30) {
            0 => "y".repeat(300),
            1 => "z".repeat(70_000),
            _ => WORDS[rng.below(WORDS.len() as u64) as usize].to_owned(),
        }
    }

    /// A random `AnyValue`, nested `depth` deep at the most.
    fn any(rng: &mut crate::testkit::Rng, w: &mut Writer, depth: u32) {
        match rng.below(if depth == 0 { 5 } else { 7 }) {
            0 => w.str(1, &word(rng)),
            1 => w.varint(2, rng.below(2)),
            2 => {
                let n = rng.next_u64() >> rng.below(64);
                w.varint(3, n);
            }
            3 => w.double(4, [0.5, -1.25, 1e300, f64::NAN, 3.0][rng.below(5) as usize]),
            4 => w.bytes(7, &[1, 2, 255]),
            5 => w.message(5, |a| {
                for _ in 0..rng.below(4) {
                    a.message(1, |v| any(rng, v, depth - 1));
                }
            }),
            _ => w.message(6, |m| {
                for _ in 0..rng.below(4) {
                    m.message(1, |kv| {
                        kv.str(1, &word(rng));
                        kv.message(2, |v| any(rng, v, depth - 1));
                    });
                }
            }),
        }
    }

    fn key_value(rng: &mut crate::testkit::Rng, kv: &mut Writer) {
        if rng.below(10) > 0 {
            kv.str(1, &word(rng));
        }
        if rng.below(10) > 0 {
            kv.message(2, |v| any(rng, v, 2));
        }
    }

    /// A random request: resources (said once, twice or not at all), scopes (named or not, more than
    /// once), records with and without each of their fields, attributes past `MAX_TAGS`, ids of
    /// every length, errors that are issues.
    fn request(rng: &mut crate::testkit::Rng) -> Vec<u8> {
        let mut w = Writer::default();
        for _ in 0..1 + rng.below(2) {
            w.message(1, |rl| {
                for _ in 0..rng.below(3) {
                    rl.message(1, |res| {
                        for _ in 0..rng.below(12) {
                            res.message(1, |kv| key_value(rng, kv));
                        }
                    });
                }
                for _ in 0..1 + rng.below(3) {
                    rl.message(2, |sl| {
                        for _ in 0..rng.below(3) {
                            sl.message(1, |scope| {
                                for _ in 0..rng.below(3) {
                                    scope.str(1, &word(rng));
                                }
                            });
                        }
                        for _ in 0..rng.below(20) {
                            sl.message(2, |r| {
                                if rng.below(4) > 0 {
                                    r.fixed64(1, 1_700_000_000_000_000_000 + rng.below(1 << 40));
                                }
                                if rng.below(2) == 0 {
                                    r.fixed64(11, 1_700_000_000_000_000_000 + rng.below(1 << 40));
                                }
                                if rng.below(3) > 0 {
                                    r.varint(2, rng.below(30));
                                }
                                if rng.below(2) == 0 {
                                    r.str(3, &word(rng));
                                }
                                if rng.below(5) > 0 {
                                    r.message(5, |b| any(rng, b, 2));
                                }
                                for _ in 0..[0, 1, 3, 8, 70][rng.below(5) as usize] {
                                    r.message(6, |kv| key_value(rng, kv));
                                }
                                let id = |rng: &mut crate::testkit::Rng, n: usize| -> Vec<u8> {
                                    let len = [n, n - 1, 0, n][rng.below(4) as usize];
                                    (0..len).map(|_| if rng.below(8) == 0 { 0 } else { rng.below(256) as u8 }).collect()
                                };
                                if rng.below(3) > 0 {
                                    let t = id(rng, 16);
                                    r.bytes(9, &t);
                                }
                                if rng.below(3) > 0 {
                                    let s = id(rng, 8);
                                    r.bytes(10, &s);
                                }
                                if rng.below(4) == 0 {
                                    r.str(12, &word(rng));
                                }
                            });
                        }
                    });
                }
            });
        }
        w.buf
    }

    /// Records decoded straight into rows are the rows `decode`'s records write, each error's issue
    /// on it as the owner puts one, read back as the store reads them; those a pipeline reads are
    /// `decode`'s records themselves, in their places; a request is taken or refused alike, however
    /// it is cut or corrupted.
    #[test]
    fn rows_decoded_are_the_rows_decode_s_records_write() {
        let mut rng = crate::testkit::Rng::new(31);
        let mut d = Decoder::new();
        let (mut rows, mut records) = (0, 0);
        for i in 0..600 {
            let req = request(&mut rng);
            let theirs = decode(&req, 42).unwrap();
            // In every other request, every third record one a pipeline reads.
            let mut n = 0u64;
            let mut out = Decoded::default();
            d.decode(
                &req,
                42,
                &mut |_| {
                    n += 1;
                    i % 2 == 0 && n.is_multiple_of(3)
                },
                &mut out,
            )
            .unwrap();
            assert_eq!((out.records, out.dropped_attrs), (theirs.records.len() as u64, theirs.dropped_attrs));
            let mut got = Vec::new();
            for part in &out.parts {
                match part {
                    LogPart::Records(r) => got.extend(r.iter().map(|r| (r.clone(), false))),
                    LogPart::Rows { raw, rows, last_ts } => {
                        let mut it = crate::logs::record::Rows::new(raw, *rows as usize);
                        let mut last = 0;
                        while let Some(r) = it.next_row().unwrap() {
                            last = r.ts;
                            got.push((r.to_owned(), true));
                        }
                        assert_eq!(last, *last_ts);
                    }
                }
            }
            assert_eq!(got.len(), theirs.records.len(), "request {i}");
            for (k, ((mine, row), theirs)) in got.iter().zip(&theirs.records).enumerate() {
                assert_eq!(*row, !(i % 2 == 0 && (k + 1) % 3 == 0), "record {k} of request {i}: in its place");
                let want = if *row {
                    let mut r = theirs.clone();
                    crate::errors::put_log_issue(&mut r);
                    let mut raw = Vec::new();
                    crate::logs::record::put_row(&mut raw, &r, 0);
                    rows += 1;
                    crate::logs::record::Rows::new(&raw, 1).next_row().unwrap().unwrap().to_owned()
                } else {
                    records += 1;
                    theirs.clone()
                };
                assert_eq!(mine, &want, "record {k} of request {i}");
            }
            for part in out.parts {
                if let LogPart::Rows { raw, .. } = part {
                    d.give_rows(raw);
                }
            }
        }
        assert!(rows > 3000 && records > 300, "{rows} rows, {records} records");
        for _ in 0..3000 {
            let mut bad = request(&mut rng);
            if bad.is_empty() {
                continue;
            }
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] = rng.next_u64() as u8;
            let bad = &bad[..rng.below(bad.len() as u64 + 1) as usize];
            assert_eq!(decode(bad, 0).is_ok(), d.decode(bad, 0, &mut |_| false, &mut Decoded::default()).is_ok());
        }
    }

    #[test]
    fn records_take_the_resource_s_service_and_host_and_their_own_fields() {
        let mut req = Writer::default();
        req.message(1, |rl| {
            rl.message(1, |res| {
                kv(res, 1, "service.name", "checkout");
                kv(res, 1, "host.name", "web-1");
                kv(res, 1, "deployment.environment", "prod");
            });
            rl.message(2, |sl| {
                sl.message(1, |scope| scope.str(1, "io.opentelemetry.http"));
                sl.message(2, |r| {
                    r.fixed64(1, 1_700_000_000_123_456_789);
                    r.varint(2, 17);
                    r.str(3, "ERROR");
                    r.message(5, |b| b.str(1, "payment failed: card declined"));
                    kv(r, 6, "http.status_code", "502");
                    r.bytes(9, &[7; 16]);
                    r.bytes(10, &[9; 8]);
                    r.str(12, "payment.failed");
                });
                sl.message(2, |r| {
                    // No time of its own: the observed time; severity from its text; a body that is a map.
                    r.fixed64(11, 1_700_000_001_000_000_000);
                    r.str(3, "warning");
                    r.message(5, |b| {
                        b.message(6, |kvl| {
                            kvl.message(1, |e| {
                                e.str(1, "user");
                                e.message(2, |a| a.str(1, "ada"));
                            });
                        });
                    });
                    r.bytes(9, &[]);
                });
            });
        });
        let d = decode(&req.buf, 42).unwrap();
        assert_eq!(d.records.len(), 2);
        let a = &d.records[0];
        assert_eq!((a.ts, a.severity, a.service.as_str(), a.host.as_str(), a.source.as_str()), (1_700_000_000_123_456_789, 17, "checkout", "web-1", "io.opentelemetry.http"));
        assert_eq!(a.message, "payment failed: card declined");
        assert_eq!(a.trace_id, Some([7; 16]));
        assert_eq!(a.span_id, Some([9; 8]));
        assert!(a.attrs.contains(&("http.status_code".into(), "502".into())));
        assert!(a.attrs.contains(&("deployment.environment".into(), "prod".into())));
        assert!(a.attrs.contains(&("event.name".into(), "payment.failed".into())));
        assert!(!a.attrs.iter().any(|(k, _)| k == "service.name" || k == "host.name"));
        let b = &d.records[1];
        assert_eq!((b.ts, b.severity), (1_700_000_001_000_000_000, 13));
        assert_eq!(b.message, r#"{"user":"ada"}"#);
        assert_eq!(b.trace_id, None);
        // A malformed request is an error, never a panic.
        let mut rng = crate::testkit::Rng::new(5);
        for _ in 0..5000 {
            let mut bad = req.buf.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] = rng.next_u64() as u8;
            let _ = decode(&bad[..rng.below(bad.len() as u64 + 1) as usize], 0);
        }
    }
}
