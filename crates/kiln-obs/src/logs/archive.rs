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
//! What an archive writes of a file of records is a function of the file (its checksum, size and
//! records' times) and the archive's id alone: each event's `_id` (its time, eight bytes of the two
//! hashed, its place in the file), each object's name. A file written again (a retry, a restart,
//! the store's files numbered anew) is written over itself, never twice. An object holds events of
//! one hour (of their own times, as Vector's sink partitioned them): what reads an hour's folder
//! finds every event of that hour.

use std::io::Write as _;

use sha2::{Digest, Sha256};

use super::record::{RecordRef, level};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{Record, Rows, put_row};

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
    }
}
