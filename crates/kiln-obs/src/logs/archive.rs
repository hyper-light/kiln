//! Log archives (`docs/observe.md` §10.5): records written as Datadog's Log Archives hold them, so
//! what reads Datadog's archives reads kiln's: JSON lines, one event a line (gzip'd by the writer,
//! `kiln::observe::archives`), under `<prefix>/dt=<YYYYMMDD>/hour=<HH>/archive_<HHmmss.SSSS>.<uuid>.json.gz`.
//!
//! An event, as Datadog's Observability Pipelines Worker writes one for Datadog's rehydration (and
//! Vector's `datadog_archives` sink did): `_id`, `date` (RFC 3339 to the millisecond, UTC), `host`,
//! `source`, `service`, `status`, `message`, `tags`, `trace_id` and `span_id` at the top; every
//! other attribute under `attributes`, a dotted key a path of objects (`http.status_code`:
//! `{"http":{"status_code":…}}`) unless a key is both a value and another's path (then each of them
//! stays as it is, flat). An attribute's value is the string kiln holds. kiln keeps no tags apart
//! from attributes (an Agent's `ddtags` are its attributes): `tags` holds, when asked, the record's
//! unified service tags (`service`, `host`, `source`, and `env` and `version` where its attributes
//! say them).
//!
//! [`read_event`] reads an event back as its record (a rehydration's, `kiln::observe::rehydrate`),
//! [`event`]'s and Datadog's own alike, in place: nothing allocated an event once its room has
//! room.
//!
//! What an archive writes of a file of records is a function of the file (its checksum, size and
//! records' times) and the archive's id alone: each event's `_id` (its time, eight bytes of the two
//! hashed, its place in the file), each object's name. A file written again (a retry, a restart,
//! the store's files numbered anew) is written over itself, never twice. An object holds events of
//! one hour (of their own times, as Vector's sink partitioned them): what reads an hour's folder
//! finds every event of that hour.

use std::io::Write as _;

use sha2::{Digest, Sha256};

use super::record::{Record, RecordRef, level};
use crate::json::{Str, Val};

/// Attributes a record's event holds on the stack (most records have fewer): past them, a vector.
const FEW: usize = 32;

/// Bytes of events an object holds at the most (uncompressed) before the next begins: Datadog's
/// (its Observability Pipelines' archives destination, and Vector's sink: 100 MB a batch); an event
/// larger is an object of its own.
pub const PART_BYTES: usize = 100_000_000;

/// Eight bytes of `archive` (its id) and a file (its checksum, size, and its records' first and
/// last times): what an event's `_id` and an object's name are made of, so the same file writes the
/// same archive's objects alike.
pub fn seed(archive: &str, file_crc: u32, file_bytes: u64, min_ts: i64, max_ts: i64) -> [u8; 8] {
    let mut h = Sha256::new();
    h.update(b"kiln archive\0");
    h.update(archive.as_bytes());
    h.update([0]);
    h.update(file_crc.to_be_bytes());
    h.update(file_bytes.to_be_bytes());
    h.update(min_ts.to_be_bytes());
    h.update(max_ts.to_be_bytes());
    let d = h.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(d.get(..8).unwrap_or(&[0; 8]));
    out
}

/// An event's `_id`, Datadog's own shape: 18 bytes, base64'd (24 characters): its time (ms, six
/// bytes big-endian), eight bytes of the writer (here the archive's and the file's, [`seed`]), and a
/// counter (four bytes: the record's place in the file).
pub fn id(ts_ms: i64, seed: [u8; 8], n: u32, out: &mut String) {
    let mut b = [0u8; 18];
    let t = ts_ms.to_be_bytes();
    for (dst, src) in b.iter_mut().zip(t.get(2..).unwrap_or(&[])) {
        *dst = *src;
    }
    for (dst, src) in b.iter_mut().skip(6).zip(seed) {
        *dst = src;
    }
    for (dst, src) in b.iter_mut().skip(14).zip(n.to_be_bytes()) {
        *dst = src;
    }
    base64(&b, out);
}

/// Standard base64 (RFC 4648 §4), with its padding.
fn base64(bytes: &[u8], out: &mut String) {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let c = |i: u32| char::from(*A.get((i & 63) as usize).unwrap_or(&b'A'));
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &x)| n | (u32::from(x) << (16 - 8 * i)));
        out.push(c(n >> 18));
        out.push(c(n >> 12));
        out.push(if chunk.len() > 1 { c(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { c(n) } else { '=' });
    }
}

/// An object's UUID: RFC 9562's version 8 (its bits a writer's own), of [`seed`], the hour its
/// events are of (hours since the epoch) and its part of them, hex in its 8-4-4-4-12 form.
pub fn uuid(seed: [u8; 8], hour: i64, part: u32) -> String {
    let mut h = Sha256::new();
    h.update(b"kiln archive object\0");
    h.update(seed);
    h.update(hour.to_be_bytes());
    h.update(part.to_be_bytes());
    let d = h.finalize();
    let mut u = [0u8; 16];
    u.copy_from_slice(d.get(..16).unwrap_or(&[0; 16]));
    if let Some(v) = u.get_mut(6) {
        *v = (*v & 0x0f) | 0x80;
    }
    if let Some(v) = u.get_mut(8) {
        *v = (*v & 0x3f) | 0x80;
    }
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    let part = |a: usize, b: usize| hex.get(a..b).unwrap_or("");
    format!("{}-{}-{}-{}-{}", part(0, 8), part(8, 12), part(12, 16), part(16, 20), part(20, 32))
}

/// An object's key: `<prefix>/dt=<YYYYMMDD>/hour=<HH>/archive_<HHmmss.SSSS>.<uuid>.json.gz`, its time
/// `at_ms`'s (UTC; the writer gives its newest event's, so an object lies in its events' hour),
/// `SSSS` its milliseconds and a tenth; a prefix's slashes at either end, and none, alike.
pub fn key(prefix: &str, at_ms: i64, uuid: &str) -> String {
    let (days, ms) = (at_ms.div_euclid(86_400_000), at_ms.rem_euclid(86_400_000));
    let (y, m, d) = crate::query::calendar::civil_from_days(days);
    let (h, min, s, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1_000 % 60, ms % 1_000);
    let prefix = prefix.trim_matches('/');
    let lead = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    format!("{lead}dt={y:04}{m:02}{d:02}/hour={h:02}/archive_{h:02}{min:02}{s:02}.{milli:03}0.{uuid}.json.gz")
}

/// The folder an hour's objects lie in (hours since the epoch, UTC):
/// `<prefix>/dt=<YYYYMMDD>/hour=<HH>/`, what [`key`] begins with for a time of that hour.
pub fn folder(prefix: &str, hour: i64) -> String {
    format!("{}hour={:02}/", day_folder(prefix, hour.div_euclid(24)), hour.rem_euclid(24))
}

/// The folder a day's hours' folders lie in (days since the epoch, UTC): `<prefix>/dt=<YYYYMMDD>/`.
pub fn day_folder(prefix: &str, day: i64) -> String {
    let (y, m, d) = crate::query::calendar::civil_from_days(day);
    let prefix = prefix.trim_matches('/');
    let lead = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    format!("{lead}dt={y:04}{m:02}{d:02}/")
}

/// The hour (since the epoch, UTC) a record of `ts` (ns) is archived under.
pub fn hour_of(ts: i64) -> i64 {
    ts.div_euclid(3_600_000_000_000)
}

/// What [`event`] reuses from record to record: a record's attributes, sorted by their paths.
#[derive(Debug, Default)]
pub struct Scratch {
    order: Vec<usize>,
    flat: Vec<bool>,
}

/// One event of `r` (a line, its newline last) onto `out`: `_id` given (see [`id`]); `tags` the
/// record's unified service tags when `tags`. Nothing allocated but `out`'s growth, for a record of
/// [`FEW`] attributes or fewer.
pub fn event(r: &RecordRef<'_>, id: &str, tags: bool, out: &mut Vec<u8>, scratch: &mut Scratch) {
    let mut few: [(&str, &str); FEW] = [("", ""); FEW];
    let mut many: Vec<(&str, &str)> = Vec::new();
    let mut n = 0;
    for kv in r.attrs() {
        match few.get_mut(n) {
            Some(slot) => *slot = kv,
            None => {
                if many.is_empty() {
                    many.extend_from_slice(&few);
                }
                many.push(kv);
            }
        }
        n += 1;
    }
    let attrs: &[(&str, &str)] = if n <= FEW { few.get(..n).unwrap_or(&[]) } else { &many };
    out.extend_from_slice(b"{\"_id\":");
    string(id, out);
    out.extend_from_slice(b",\"date\":");
    date(r.ts.div_euclid(1_000_000), out);
    for (k, v) in [("host", r.host), ("source", r.source), ("service", r.service), ("status", level(r.severity)), ("message", r.message)] {
        out.push(b',');
        string(k, out);
        out.push(b':');
        string(v, out);
    }
    out.extend_from_slice(b",\"tags\":[");
    if tags {
        let attr = |k: &str| attrs.iter().find(|(a, _)| *a == k).map(|(_, v)| *v);
        let mut first = true;
        let named = [("service", Some(r.service)), ("host", Some(r.host)), ("source", Some(r.source)), ("env", attr("env")), ("version", attr("version"))];
        for (k, v) in named {
            let Some(v) = v.filter(|v| !v.is_empty()) else { continue };
            if !first {
                out.push(b',');
            }
            first = false;
            out.push(b'"');
            escaped(k, out);
            out.push(b':');
            escaped(v, out);
            out.push(b'"');
        }
    }
    out.push(b']');
    if let Some(t) = r.trace_id {
        out.extend_from_slice(b",\"trace_id\":");
        hex(&t, out);
    }
    if let Some(s) = r.span_id {
        out.extend_from_slice(b",\"span_id\":");
        hex(&s, out);
    }
    out.extend_from_slice(b",\"attributes\":");
    attributes(attrs, out, scratch);
    out.extend_from_slice(b"}\n");
}

/// `ms` as RFC 3339 to the millisecond, UTC, quoted (written in place: nothing allocated).
fn date(ms: i64, out: &mut Vec<u8>) {
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = crate::query::calendar::civil_from_days(days);
    let _ = write!(out, "\"{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z\"", rest / 3_600_000, rest / 60_000 % 60, rest / 1_000 % 60, rest % 1_000);
}

/// Bytes as lowercase hex, quoted.
fn hex(b: &[u8], out: &mut Vec<u8>) {
    const H: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for &x in b {
        out.push(*H.get(usize::from(x >> 4)).unwrap_or(&b'0'));
        out.push(*H.get(usize::from(x & 15)).unwrap_or(&b'0'));
    }
    out.push(b'"');
}

/// `s` as a JSON string (RFC 8259 §7): `"` and `\` escaped, a control character as its short escape
/// or `\u00XX`; the rest as it is (UTF-8).
pub fn string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    escaped(s, out);
    out.push(b'"');
}

/// `s` escaped as a JSON string's characters are, without its quotes.
fn escaped(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let mut from = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let short: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(bytes.get(from..i).unwrap_or(&[]));
        if short.is_empty() {
            const H: &[u8; 16] = b"0123456789abcdef";
            out.extend_from_slice(b"\\u00");
            out.push(*H.get(usize::from(b >> 4)).unwrap_or(&b'0'));
            out.push(*H.get(usize::from(b & 15)).unwrap_or(&b'0'));
        } else {
            out.extend_from_slice(short);
        }
        from = i + 1;
    }
    out.extend_from_slice(bytes.get(from..).unwrap_or(&[]));
}

/// Objects an attribute's path opens at the most: a key of more parts is kept flat.
const DEPTH: usize = 16;

/// Whether `k` splits into a path: no part of it empty, [`DEPTH`] parts at the most.
fn path_of(k: &str) -> bool {
    !k.is_empty() && k.split('.').all(|p| !p.is_empty()) && k.split('.').count() <= DEPTH
}

/// `attrs` as an object: each nestable key a path of objects, in their paths' order (each key's
/// first value of keys given twice); a key that is both a value and another's path, and what lies
/// under it, flat, after them.
fn attributes(attrs: &[(&str, &str)], out: &mut Vec<u8>, scratch: &mut Scratch) {
    let Scratch { order, flat, .. } = scratch;
    order.clear();
    order.extend(0..attrs.len());
    let key = |i: usize| attrs.get(i).map_or("", |a| a.0);
    // By path: part by part, so `a` < `a.b` < `a-b` (a key's paths beneath it right after it).
    order.sort_by(|&a, &b| key(a).split('.').cmp(key(b).split('.')).then(a.cmp(&b)));
    // A key given twice: its first.
    order.dedup_by(|b, a| key(*a) == key(*b));
    flat.clear();
    flat.extend(order.iter().map(|&i| !path_of(key(i))));
    // A key that is a path's beginning (`a` before `a.b`): it and every key beneath it, flat.
    let mut i = 0;
    while i < order.len() {
        let k = key(order.get(i).copied().unwrap_or(0));
        let beneath = |j: usize| order.get(j).is_some_and(|&o| key(o).strip_prefix(k).is_some_and(|rest| rest.starts_with('.')));
        if beneath(i + 1) {
            let mut j = i;
            while j == i || beneath(j) {
                if let Some(f) = flat.get_mut(j) {
                    *f = true;
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out.push(b'{');
    // The nested ones, objects opened and closed as their paths go (the open ones on the stack).
    let mut open: [&str; DEPTH] = [""; DEPTH];
    let mut depth = 0;
    let mut first = true;
    for (n, &i) in order.iter().enumerate() {
        if flat.get(n).copied().unwrap_or(true) {
            continue;
        }
        let Some((k, v)) = attrs.get(i) else { continue };
        let (dirs, leaf) = k.rsplit_once('.').map_or(("", *k), |(d, l)| (d, l));
        let parts = || dirs.split('.').filter(|d| !d.is_empty());
        let shared = open.iter().take(depth).zip(parts()).take_while(|(a, b)| *a == b).count();
        while depth > shared {
            depth -= 1;
            out.push(b'}');
            first = false;
        }
        for d in parts().skip(shared) {
            if !first {
                out.push(b',');
            }
            string(d, out);
            out.extend_from_slice(b":{");
            if let Some(slot) = open.get_mut(depth) {
                *slot = d;
            }
            depth += 1;
            first = true;
        }
        if !first {
            out.push(b',');
        }
        string(leaf, out);
        out.push(b':');
        string(v, out);
        first = false;
    }
    while depth > 0 {
        depth -= 1;
        out.push(b'}');
        first = false;
    }
    for (n, &i) in order.iter().enumerate() {
        if !flat.get(n).copied().unwrap_or(false) {
            continue;
        }
        let Some((k, v)) = attrs.get(i) else { continue };
        if !first {
            out.push(b',');
        }
        string(k, out);
        out.push(b':');
        string(v, out);
        first = false;
    }
    out.push(b'}');
}

// ------------------------------------------------------------------------------------- reading

/// Attributes an event read back keeps at the most (its tags among them).
pub const READ_ATTRS: usize = 256;

/// Strings of attributes let go that [`ReadRoom`] keeps for the next ones, at the most.
const SPARE: usize = 1_024;

/// What [`read_event`] reuses from event to event: the fields of each depth's object (their room
/// kept from line to line), the JSON reader's, a key's path, text decoded, and the strings of
/// attributes let go (taken again before any is made).
#[derive(Debug, Default)]
pub struct ReadRoom {
    levels: Vec<Vec<(Str<'static>, Val<'static>)>>,
    json: crate::json::Scratch,
    path: String,
    buf: String,
    spare: Vec<(String, String)>,
}

/// Lists of fields emptied, as lists of borrows of another lifetime, their room kept (collected in
/// place: the same layout, nothing allocated).
fn levels_for<'b>(v: Vec<Vec<(Str<'_>, Val<'_>)>>) -> Vec<Vec<(Str<'b>, Val<'b>)>> {
    v.into_iter()
        .map(|mut l| {
            l.clear();
            l.into_iter().map(|_| (Str { raw: "", escaped: false }, Val::Null)).collect()
        })
        .collect()
}

/// An archived event (a line of an object: what [`event`] writes, or Datadog's own archives' events)
/// read back into `r`, written over what it held (its strings' room kept): `date` its time (RFC
/// 3339, or ms since the epoch); `host`, `source`, `service` and `message` its own; `status` a
/// level's name (none: info); every field under `attributes`, and any other at the top, an
/// attribute, nested objects' keys joined by dots ([`DEPTH`] deep: deeper, as JSON), a string as it
/// is, a number as written, an array as its JSON, null left out; each of its `tags` (`key:value`) an
/// attribute where none of its key is (`service`, `host` and `source` being the record's own);
/// `trace_id` and `span_id` hex, or a tracer's decimal 64-bit ids (Datadog's `dd.trace_id` and
/// `dd.span_id` attributes where those are not). [`READ_ATTRS`] attributes at the most. False (`r`
/// as it may be) for a line that is no event: not a JSON object, or no time. Nothing allocated but
/// strings longer than those they are written over, once its room has room.
pub fn read_event(line: &[u8], r: &mut Record, room: &mut ReadRoom) -> bool {
    let Ok(text) = std::str::from_utf8(line) else { return false };
    let ReadRoom { levels, json, path, buf, spare } = room;
    let mut pool = levels_for(std::mem::take(levels));
    if pool.len() <= DEPTH {
        pool.resize_with(DEPTH + 1, Vec::new);
    }
    let read = read_fields(text, r, &mut pool, json, path, buf, spare);
    *levels = levels_for(pool);
    read
}

/// An attribute added to `r` (an event read back) where none of its key is (a rehydration's tags),
/// its strings those [`read_event`] let go when there are.
pub fn add_attr(r: &mut Record, k: &str, v: &str, room: &mut ReadRoom) {
    if r.attrs.len() >= READ_ATTRS || r.attrs.iter().any(|(a, _)| a == k) {
        return;
    }
    let (mut key, mut val) = room.spare.pop().unwrap_or_default();
    key.clear();
    key.push_str(k);
    val.clear();
    val.push_str(v);
    r.attrs.push((key, val));
}

fn read_fields<'a>(
    text: &'a str,
    r: &mut Record,
    pool: &mut [Vec<(Str<'a>, Val<'a>)>],
    json: &mut crate::json::Scratch,
    path: &mut String,
    buf: &mut String,
    spare: &mut Vec<(String, String)>,
) -> bool {
    let Some((top, nested)) = pool.split_first_mut() else { return false };
    if crate::json::object(text, top, json).is_err() {
        return false;
    }
    let (mut ts, mut tags) = (None, None);
    r.severity = 9;
    (r.trace_id, r.span_id) = (None, None);
    for s in [&mut r.service, &mut r.host, &mut r.source, &mut r.message] {
        s.clear();
    }
    let mut attrs = Attrs { v: &mut r.attrs, spare, n: 0 };
    for &(k, v) in top.iter() {
        // An event's own keys are plain words: an escaped key is none of them.
        match if k.escaped { "" } else { k.raw } {
            "date" => ts = time_of(v, buf),
            "host" => text_into(v, &mut r.host),
            "source" => text_into(v, &mut r.source),
            "service" => text_into(v, &mut r.service),
            "message" => text_into(v, &mut r.message),
            "status" => r.severity = severity_of(v, buf),
            "tags" => tags = Some(v),
            "trace_id" => r.trace_id = id_of(v, buf),
            "span_id" => r.span_id = id_of(v, buf),
            "_id" => {}
            "attributes" => {
                path.clear();
                flatten(path, v, 0, nested, json, &mut attrs);
            }
            _ => {
                path.clear();
                k.push_to(path);
                flatten(path, v, 1, nested, json, &mut attrs);
            }
        }
    }
    if let Some(Val::Array(raw)) = tags {
        crate::json::strings_of(raw, |t| {
            let t = t.text(buf).trim();
            let (k, v) = t.split_once(':').unwrap_or((t, ""));
            if !k.is_empty() && !matches!(k, "service" | "host" | "source") && attrs.get(k).is_none() {
                attrs.push(k, |out| out.push_str(v));
            }
        });
    }
    if r.trace_id.is_none() {
        r.trace_id = attrs.get("dd.trace_id").and_then(|s| id_text(s.trim()));
    }
    if r.span_id.is_none() {
        r.span_id = attrs.get("dd.span_id").and_then(|s| id_text(s.trim()));
    }
    attrs.finish();
    match ts {
        Some(ts) => {
            r.ts = ts;
            true
        }
        None => false,
    }
}

/// A record's attributes written over those it had (each slot's strings' room kept); those left
/// over let go into `spare`, which a slot past them takes from before one is made.
struct Attrs<'r> {
    v: &'r mut Vec<(String, String)>,
    spare: &'r mut Vec<(String, String)>,
    n: usize,
}

impl Attrs<'_> {
    fn push(&mut self, k: &str, value: impl FnOnce(&mut String)) {
        if self.n >= READ_ATTRS {
            return;
        }
        if self.n == self.v.len() {
            self.v.push(self.spare.pop().unwrap_or_default());
        }
        if let Some((key, val)) = self.v.get_mut(self.n) {
            key.clear();
            key.push_str(k);
            val.clear();
            value(val);
            self.n += 1;
        }
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.v.iter().take(self.n).find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    /// The attributes written kept; the rest let go.
    fn finish(self) {
        if self.v.len() > self.n {
            let room = SPARE.saturating_sub(self.spare.len());
            self.spare.extend(self.v.drain(self.n..).take(room));
        }
    }
}

/// `v` as attributes under `path` (nested objects' keys joined by dots, their fields read into
/// `pool`'s lists, [`DEPTH`] deep: deeper, as JSON): a string as it is, a number as written, a
/// literal as its word, an array as its JSON; null left out, and a value with no key.
fn flatten<'a>(path: &mut String, v: Val<'a>, level: usize, pool: &mut [Vec<(Str<'a>, Val<'a>)>], json: &mut crate::json::Scratch, out: &mut Attrs<'_>) {
    match v {
        Val::Object(raw) if level < DEPTH => {
            let Some((list, rest)) = pool.split_first_mut() else { return };
            // Checked whole with the line already: it reads.
            if crate::json::object_at(raw, level + 2, list, json).is_err() {
                return;
            }
            let at = path.len();
            for &(k, v) in list.iter() {
                path.truncate(at);
                if at > 0 {
                    path.push('.');
                }
                k.push_to(path);
                flatten(path, v, level + 1, rest, json, out);
            }
            path.truncate(at);
        }
        Val::Null => {}
        _ if path.is_empty() => {}
        Val::Str(s) => out.push(path, |t| s.push_to(t)),
        Val::Num(n) => out.push(path, |t| t.push_str(n)),
        Val::Bool(b) => out.push(path, |t| t.push_str(if b { "true" } else { "false" })),
        Val::Array(raw) | Val::Object(raw) => out.push(path, |t| crate::json::compact(raw, t)),
    }
}

/// A value's text into `out`: a string's, decoded; a number as written; a literal's word; an array
/// or object as its JSON; null, none.
fn text_into(v: Val<'_>, out: &mut String) {
    out.clear();
    match v {
        Val::Str(s) => s.push_to(out),
        Val::Num(n) => out.push_str(n),
        Val::Bool(b) => out.push_str(if b { "true" } else { "false" }),
        Val::Null => {}
        Val::Array(raw) | Val::Object(raw) => crate::json::compact(raw, out),
    }
}

/// A status as a severity: a level's name (Datadog's, syslog's, OpenTelemetry's), the first of its
/// range; none (or empty), info; another, unspecified.
fn severity_of(v: Val<'_>, buf: &mut String) -> u8 {
    let s = match v {
        Val::Str(s) => s.text(buf),
        Val::Null => return 9,
        _ => return 0,
    };
    if s.is_empty() {
        return 9;
    }
    super::record::severities(s).map_or(0, |r| *r.start())
}

/// A time as ns since the epoch: RFC 3339, or a whole number of ms.
fn time_of(v: Val<'_>, buf: &mut String) -> Option<i64> {
    match v {
        Val::Str(s) => rfc3339_ns(s.text(buf)),
        Val::Num(n) => n.parse::<i64>().ok()?.checked_mul(1_000_000),
        _ => None,
    }
}

/// RFC 3339's time as ns since the epoch: its usual form (`2018-05-15T14:31:16.003Z`, what Datadog's
/// archives and [`event`] write) read in place; any other (an offset, more or fewer digits of a
/// second, a lowercase `t`) by jiff's reading.
fn rfc3339_ns(s: &str) -> Option<i64> {
    if let Ok(&[y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1, b'T', h0, h1, b':', i0, i1, b':', s0, s1, b'.', f0, f1, f2, b'Z']) = <&[u8; 24]>::try_from(s.as_bytes()) {
        let num = |ds: &[u8]| ds.iter().try_fold(0i64, |n, &c| c.is_ascii_digit().then_some(n * 10 + i64::from(c.wrapping_sub(b'0'))));
        let (y, m, d) = (num(&[y0, y1, y2, y3])?, num(&[m0, m1])?, num(&[d0, d1])?);
        let (h, mi, sec, ms) = (num(&[h0, h1])?, num(&[i0, i1])?, num(&[s0, s1])?, num(&[f0, f1, f2])?);
        if !(1..=12).contains(&m) || d < 1 || d > crate::query::calendar::days_in_month(y, m) || h > 23 || mi > 59 || sec > 60 {
            return None;
        }
        // A leap second its minute's last, as jiff reads one.
        let secs = crate::query::calendar::days_from_civil(y, m, d) * 86_400 + h * 3_600 + mi * 60 + sec.min(59);
        return secs.checked_mul(1_000)?.checked_add(ms)?.checked_mul(1_000_000);
    }
    let t: jiff::Timestamp = s.trim().parse().ok()?;
    i64::try_from(t.as_nanosecond()).ok()
}

/// An id of `N` bytes from a value: see [`id_text`].
fn id_of<const N: usize>(v: Val<'_>, buf: &mut String) -> Option<[u8; N]> {
    match v {
        Val::Str(s) => id_text(s.text(buf).trim()),
        Val::Num(n) => id_text(n),
        _ => None,
    }
}

/// An id of `N` bytes, right-aligned: hex of its width (or of a tracer's 64-bit one, 16 digits, or
/// fewer), or digits alone a decimal 64-bit id (a Datadog tracer's). None for what is none of these.
fn id_text<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    let mut out = [0u8; N];
    let hex_width = b.len() == 2 * N || b.len() == 16;
    if !hex_width && !b.is_empty() && b.len() <= 20 && b.iter().all(u8::is_ascii_digit) {
        let n: u64 = s.parse().ok()?;
        for (d, x) in out.iter_mut().rev().zip(n.to_be_bytes().iter().rev()) {
            *d = *x;
        }
        return Some(out);
    }
    if b.is_empty() || !b.len().is_multiple_of(2) || b.len() > 2 * N {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    for (slot, &[hi, lo]) in out.iter_mut().skip(N - b.len() / 2).zip(b.as_chunks::<2>().0) {
        *slot = (nibble(hi)? << 4) | nibble(lo)?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{Rows, put_row};

    fn rows(records: &[Record]) -> Vec<u8> {
        let (mut raw, mut prev) = (Vec::new(), 0);
        for r in records {
            put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        raw
    }

    fn one(r: &Record, tags: bool) -> serde_json::Value {
        let raw = rows(std::slice::from_ref(r));
        let got = Rows::new(&raw, 1).next_row().unwrap().unwrap();
        let mut out = Vec::new();
        event(&got, "AAAA", tags, &mut out, &mut Scratch::default());
        assert_eq!(out.last(), Some(&b'\n'));
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1, "one line: {}", String::from_utf8_lossy(&out));
        serde_json::from_slice(&out).unwrap()
    }

    /// An event is Datadog's archived event: its reserved attributes at the top, `date` RFC 3339 to
    /// the millisecond, the rest under `attributes` with dotted keys made paths of objects, a key
    /// that is both a value and a path's beginning (and what lies under it) flat; tags, when asked,
    /// the unified service tags; trace and span ids in hex.
    #[test]
    fn an_event_is_datadog_s_archived_event() {
        let r = Record {
            ts: 1_526_394_676_003_000_000,
            severity: 17,
            service: "web".into(),
            host: "i-123".into(),
            source: "nginx".into(),
            message: "GET /a \"quoted\"\n\tnext".into(),
            attrs: vec![
                ("http.status_code".into(), "403".into()),
                ("http.method".into(), "PUT".into()),
                ("rid".into(), "abc-123".into()),
                ("env".into(), "prod".into()),
                ("a".into(), "1".into()),
                ("a.b".into(), "2".into()),
                ("a-b".into(), "3".into()),
                ("x..y".into(), "4".into()),
                ("network.client.ip".into(), "10.0.0.1".into()),
            ],
            trace_id: Some([0xab; 16]),
            span_id: Some([0x01; 8]),
        };
        let v = one(&r, true);
        assert_eq!(v["_id"], "AAAA");
        assert_eq!(v["date"], "2018-05-15T14:31:16.003Z");
        assert_eq!((v["host"].as_str(), v["source"].as_str(), v["service"].as_str(), v["status"].as_str()), (Some("i-123"), Some("nginx"), Some("web"), Some("error")));
        assert_eq!(v["message"], "GET /a \"quoted\"\n\tnext");
        assert_eq!(v["tags"], serde_json::json!(["service:web", "host:i-123", "source:nginx", "env:prod"]));
        assert_eq!(v["trace_id"], "ab".repeat(16));
        assert_eq!(v["span_id"], "01".repeat(8));
        let a = &v["attributes"];
        assert_eq!(a["http"], serde_json::json!({ "status_code": "403", "method": "PUT" }));
        assert_eq!(a["network"]["client"]["ip"], "10.0.0.1");
        assert_eq!((a["rid"].as_str(), a["env"].as_str()), (Some("abc-123"), Some("prod")));
        // Both a value and a path: flat, both; a key of no path: flat.
        assert_eq!((a["a"].as_str(), a["a.b"].as_str(), a["a-b"].as_str(), a["x..y"].as_str()), (Some("1"), Some("2"), Some("3"), Some("4")));
        let keys: Vec<&String> = a.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 8, "{keys:?}");
        // No tags unless asked.
        assert_eq!(one(&r, false)["tags"], serde_json::json!([]));
    }

    /// A JSON string as RFC 8259 writes one: what `serde_json` reads back is the string, every
    /// character of every range (controls, quotes, backslashes, past ASCII).
    #[test]
    fn strings_are_written_as_json_reads_them() {
        let mut rng = crate::testkit::Rng::new(13);
        let pool: Vec<char> = (0u32..0x80).filter_map(char::from_u32).chain(['é', '東', '😀', '\u{2028}', '\u{fffd}']).collect();
        for _ in 0..5_000 {
            let s: String = (0..rng.below(40)).map(|_| pool[rng.below(pool.len() as u64) as usize]).collect();
            let mut out = Vec::new();
            string(&s, &mut out);
            assert_eq!(serde_json::from_slice::<String>(&out).unwrap(), s);
        }
    }

    /// Attributes in any order and any nesting come out as one object holding each once, every
    /// value where its path says (or flat where it cannot be), read back to the same pairs.
    #[test]
    fn attributes_of_any_shape_come_back_as_their_pairs() {
        let mut rng = crate::testkit::Rng::new(17);
        let parts = ["a", "b", "http", "x", "", "status_code", "a-b"];
        for _ in 0..3_000 {
            let mut attrs: Vec<(String, String)> = Vec::new();
            for i in 0..rng.below(8) {
                let k: Vec<&str> = (0..1 + rng.below(3)).map(|_| parts[rng.below(parts.len() as u64) as usize]).collect();
                attrs.push((k.join("."), format!("v{i}")));
            }
            let pairs: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let mut out = Vec::new();
            attributes(&pairs, &mut out, &mut Scratch::default());
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
            // Flattened back: each key's first value, once.
            let mut got: Vec<(String, String)> = Vec::new();
            fn walk(prefix: &str, v: &serde_json::Value, got: &mut Vec<(String, String)>) {
                for (k, x) in v.as_object().unwrap() {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    match x {
                        serde_json::Value::Object(_) => walk(&path, x, got),
                        serde_json::Value::String(s) => got.push((path, s.clone())),
                        _ => panic!("{x}"),
                    }
                }
            }
            walk("", &v, &mut got);
            let mut want: Vec<(String, String)> = Vec::new();
            for (k, x) in &attrs {
                if !want.iter().any(|(w, _)| w == k) {
                    want.push((k.clone(), x.clone()));
                }
            }
            got.sort();
            want.sort();
            assert_eq!(got, want, "{}", String::from_utf8_lossy(&out));
        }
    }

    /// Records written as events read back as themselves, each into the same record and room
    /// (its strings written over, attributes let go and taken again): every field and attribute,
    /// nested or flat, every character; tags not doubled.
    #[test]
    fn an_event_reads_back_as_its_record() {
        let mut rng = crate::testkit::Rng::new(29);
        let parts = ["a", "b", "http", "status_code", "x-y", "été", "q\"z", "a\\b"];
        let (mut back, mut room, mut line, mut scratch) = (Record::default(), ReadRoom::default(), Vec::new(), Scratch::default());
        for i in 0..3_000u64 {
            let n = rng.next_u64();
            let mut attrs: Vec<(String, String)> = Vec::new();
            for j in 0..rng.below(12) {
                let m = rng.next_u64();
                let k: Vec<&str> = (0..1 + m % 3).map(|d| parts[((m >> (8 * d)) % parts.len() as u64) as usize]).collect();
                let k = k.join(".");
                if !attrs.iter().any(|(a, _)| *a == k) {
                    attrs.push((k, format!("v{j}\n\"{}\"", m % 100)));
                }
            }
            let r = Record {
                ts: 1_800_000_000_000_000_000 + (n % 1_000_000_000) as i64 * 1_000_000,
                severity: [9, 13, 17, 21, 5, 1][(n % 6) as usize],
                service: format!("svc-{}", n % 5),
                host: if n.is_multiple_of(7) { String::new() } else { format!("h{}", n % 3) },
                source: "app".into(),
                message: format!("message {i} \u{1}\t\"quoted\" ☃"),
                attrs,
                trace_id: n.is_multiple_of(2).then_some([((n >> 8) % 256) as u8; 16]),
                span_id: n.is_multiple_of(3).then_some([((n >> 16) % 256) as u8; 8]),
            };
            let raw = rows(std::slice::from_ref(&r));
            let row = Rows::new(&raw, 1).next_row().unwrap().unwrap();
            line.clear();
            event(&row, "id", n.is_multiple_of(4), &mut line, &mut scratch);
            assert!(read_event(line.trim_ascii_end(), &mut back, &mut room), "{}", String::from_utf8_lossy(&line));
            let level = |s: u8| crate::logs::record::severities(level(s)).map_or(0, |r| *r.start());
            assert_eq!((back.ts, back.severity, &back.service, &back.host, &back.source, &back.message), (r.ts, level(r.severity), &r.service, &r.host, &r.source, &r.message));
            assert_eq!((back.trace_id, back.span_id), (r.trace_id, r.span_id));
            // Its attributes as they were; tags (when written) are its env's and version's, there already.
            let (mut want, mut got) = (r.attrs.clone(), back.attrs.clone());
            want.sort();
            got.sort();
            assert_eq!(got, want, "{}", String::from_utf8_lossy(&line));
        }
    }

    /// An event as Datadog's own archives hold one: tags as attributes, numbers as written, a
    /// tracer's decimal ids (Datadog's `dd.trace_id` and `dd.span_id` among its attributes), a
    /// status of Datadog's names; what is no event is none, and a record read after one of many
    /// attributes holds its own alone.
    #[test]
    fn datadog_s_own_archived_events_read() {
        let (mut r, mut room) = (Record::default(), ReadRoom::default());
        let line = br#"{"_id":"AAABc2dXYZ","date":"2020-08-17T15:31:24.507Z","host":"i-0abc","source":"python","service":"web","status":"warn","message":"Request took 1.2s","tags":["env:prod","service:web","team:core","version:1.4"],"trace_id":"1232391219124912494","span_id":"18446744073709551615","attributes":{"http":{"status_code":502,"method":"GET"},"duration":1.2e9,"usr":{"id":"u-1"},"ok":false,"list":[1, "a"],"gone":null}}"#;
        assert!(read_event(line, &mut r, &mut room));
        assert_eq!(r.ts, 1_597_678_284_507_000_000);
        assert_eq!((r.severity, r.service.as_str(), r.host.as_str(), r.source.as_str(), r.message.as_str()), (13, "web", "i-0abc", "python", "Request took 1.2s"));
        let attr = |r: &Record, k: &str| r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(["http.status_code", "http.method", "duration", "usr.id", "ok", "list"].map(|k| attr(&r, k)), ["502", "GET", "1.2e9", "u-1", "false", "[1,\"a\"]"].map(|v| Some(v.to_owned())));
        assert_eq!((attr(&r, "env"), attr(&r, "team"), attr(&r, "version"), attr(&r, "service"), attr(&r, "gone")), (Some("prod".into()), Some("core".into()), Some("1.4".into()), None, None));
        let mut t = [0u8; 16];
        t[8..].copy_from_slice(&1_232_391_219_124_912_494u64.to_be_bytes());
        assert_eq!((r.trace_id, r.span_id), (Some(t), Some(u64::MAX.to_be_bytes())));
        // Datadog's correlation attributes, its date in ms, its ids where the event says none.
        let line = br#"{"date":1597678284507,"message":"m","attributes":{"dd":{"trace_id":"1232391219124912494","span_id":"42"}}}"#;
        assert!(read_event(line, &mut r, &mut room));
        assert_eq!((r.ts, r.severity, r.service.as_str(), r.attrs.len()), (1_597_678_284_507_000_000, 9, "", 2));
        assert_eq!((r.trace_id, r.span_id), (Some(t), Some(42u64.to_be_bytes())));
        let line = br#"{"date":"2020-08-17T17:31:24.5+02:00","message":"m","trace_id":"0af7651916cd43dd8448eb211c80319c","span_id":"b7ad6b7169203331"}"#;
        assert!(read_event(line, &mut r, &mut room));
        assert_eq!((r.ts, r.attrs.len()), (1_597_678_284_500_000_000, 0));
        assert_eq!(r.trace_id, Some([0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31, 0x9c]));
        assert_eq!(r.span_id, Some([0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]));
        for bad in [&b"not json"[..], br#"[1,2]"#, br#"{"message":"no date"}"#, br#"{"date":"yesterday"}"#, br#"{"date":"2021-02-29T00:00:00.000Z"}"#, b"{\"date\":1} x", &[0xff, 0xfe]] {
            assert!(!read_event(bad, &mut r, &mut room), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// RFC 3339's usual form read in place is what jiff reads of it, to the nanosecond, over the
    /// years i64's nanoseconds hold (a leap second its minute's last, as jiff has it); a date that
    /// is no date is none.
    #[test]
    fn times_read_in_place_are_jiff_s() {
        let mut rng = crate::testkit::Rng::new(5);
        let (lo, hi) = (-9_223_372_036_854i64, 9_223_372_036_854i64);
        for _ in 0..200_000 {
            let ms = lo + rng.below((hi - lo) as u64) as i64;
            let mut out = Vec::new();
            date(ms, &mut out);
            let s = std::str::from_utf8(&out).unwrap().trim_matches('"');
            let jiff: jiff::Timestamp = s.parse().unwrap();
            assert_eq!(rfc3339_ns(s), i64::try_from(jiff.as_nanosecond()).ok(), "{s}");
            assert_eq!(rfc3339_ns(s), Some(ms * 1_000_000), "{s}");
        }
        assert_eq!(rfc3339_ns("2016-12-31T23:59:60.000Z"), rfc3339_ns("2016-12-31T23:59:59.000Z"));
        assert_eq!(rfc3339_ns("2016-12-31t23:59:59.5z"), rfc3339_ns("2016-12-31T23:59:59.500Z"));
        for bad in [
            "2021-02-29T00:00:00.000Z",
            "2021-13-01T00:00:00.000Z",
            "2021-12-01T24:00:00.000Z",
            "2021-12-01T00:60:00.000Z",
            "2021-12-00T00:00:00.000Z",
            "2021-1a-01T00:00:00.000Z",
            "2021-12-01T00:00:00.000",
        ] {
            assert_eq!(rfc3339_ns(bad), None, "{bad}");
        }
    }

    /// Ids as tracers write them: hex of their width, a 64-bit one's in a 128-bit id's place (right-
    /// aligned), decimals of up to 20 digits; what is none of these, none.
    #[test]
    fn ids_read_as_tracers_write_them() {
        assert_eq!(id_text::<8>("18446744073709551615"), Some([0xff; 8]));
        assert_eq!(id_text::<8>("00000000000000ff"), Some([0, 0, 0, 0, 0, 0, 0, 0xff]));
        assert_eq!(id_text::<16>("00000000000000ff"), Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]));
        assert_eq!(id_text::<16>("255"), Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]));
        assert_eq!(id_text::<16>("ABCDEF0123456789abcdef0123456789").map(|b| b[0]), Some(0xab));
        for bad in ["", "18446744073709551616", "123456789012345678901", "xyz", "abc", "0af7651916cd43dd8448eb211c80319c00"] {
            assert_eq!(id_text::<16>(bad), None, "{bad}");
        }
        assert_eq!(id_text::<8>("0af7651916cd43dd8448eb211c80319c"), None);
    }

    /// An `_id` is 18 bytes base64'd, its time and counter where Datadog's are; a seed and an
    /// object's UUID are the archive's and the file's alone (the same again, another for another, an
    /// hour's and a part's each its own), the UUID version 8 of RFC 9562; a key lies under its time's
    /// day and hour.
    #[test]
    fn ids_names_and_keys_are_the_file_s_and_the_archive_s_alone() {
        let s = seed("arch-1", 0xdead_beef, 4096, 10, 20);
        assert_eq!(s, seed("arch-1", 0xdead_beef, 4096, 10, 20));
        assert_ne!(s, seed("arch-2", 0xdead_beef, 4096, 10, 20));
        assert_ne!(s, seed("arch-1", 0xdead_beee, 4096, 10, 20));
        assert_ne!(s, seed("arch-1", 0xdead_beef, 4096, 10, 21));
        let mut out = String::new();
        id(1_526_394_676_003, s, 7, &mut out);
        assert_eq!(out.len(), 24);
        let mut padded = String::new();
        base64(b"ab", &mut padded);
        assert_eq!(padded, "YWI=");
        let u = uuid(s, 7, 0);
        assert_eq!(u, uuid(s, 7, 0));
        assert_ne!(u, uuid(s, 7, 1));
        assert_ne!(u, uuid(s, 8, 0));
        assert_eq!(hour_of(1_526_394_676_003_000_000), 17_666 * 24 + 14);
        assert_eq!(hour_of(-1), -1);
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'8', "version 8: {u}");
        assert!(matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'), "RFC 9562's variant: {u}");
        assert_eq!(key("/my/prefix/", 1_526_394_676_003, &u), format!("my/prefix/dt=20180515/hour=14/archive_143116.0030.{u}.json.gz"));
        assert_eq!(key("", 0, "u"), "dt=19700101/hour=00/archive_000000.0000.u.json.gz");
        // A key lies in its time's hour's folder, whatever the prefix's slashes.
        for (prefix, at) in [("/my/prefix/", 1_526_394_676_003i64), ("", 0), ("x", 86_400_000 * 365 + 3_599_999), ("a/b", -1)] {
            let hour = hour_of(at * 1_000_000);
            assert!(key(prefix, at, "u").starts_with(&folder(prefix, hour)), "{prefix} {at}");
        }
        assert_eq!(folder("/p/", 17_666 * 24 + 14), "p/dt=20180515/hour=14/");
        assert_eq!((day_folder("", 17_666), day_folder("a/b", -1)), ("dt=20180515/".to_owned(), "a/b/dt=19691231/".to_owned()));
    }
}
