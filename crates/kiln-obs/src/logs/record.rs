//! A log record and its row encoding (`docs/observe.md` §3, §4.6): what a write batch carries
//! (LZ4-compressed) and what a log block's text is.
//!
//! ```text
//! row := ts zigzag-varint (ns, the change from the row before) | severity u8
//!      | service | host | source | message                       (each varint-length bytes)
//!      | attrs varint | (key, value)*                             (each varint-length bytes)
//!      | flags u8 (bit 0 a trace id follows, bit 1 a span id) | trace 16 bytes | span 8 bytes
//! ```

use crate::varint;

/// The longest message kept: Datadog truncates a log's message at 1 MB
/// (docs.datadoghq.com, "Log Management: Custom Log Collection", "Logs are truncated at 1MB").
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// A log record, owned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    /// Nanoseconds since the epoch.
    pub ts: i64,
    /// OpenTelemetry's severity number, 1–24 (0: unspecified).
    pub severity: u8,
    pub service: String,
    pub host: String,
    pub source: String,
    pub message: String,
    pub attrs: Vec<(String, String)>,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
}

/// A log record read in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef<'a> {
    pub ts: i64,
    pub severity: u8,
    pub service: &'a str,
    pub host: &'a str,
    pub source: &'a str,
    pub message: &'a str,
    /// The attributes' encoding, read with [`RecordRef::attrs`].
    attrs_raw: &'a [u8],
    pub attr_count: usize,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
}

impl<'a> RecordRef<'a> {
    pub fn attrs(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let mut at = self.attrs_raw;
        (0..self.attr_count).map_while(move |_| {
            let k = get_str(&mut at)?;
            let v = get_str(&mut at)?;
            Some((k, v))
        })
    }

    pub fn attr(&self, key: &str) -> Option<&'a str> {
        self.attrs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    pub fn to_owned(&self) -> Record {
        Record {
            ts: self.ts,
            severity: self.severity,
            service: self.service.to_owned(),
            host: self.host.to_owned(),
            source: self.source.to_owned(),
            message: self.message.to_owned(),
            attrs: self.attrs().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            trace_id: self.trace_id,
            span_id: self.span_id,
        }
    }
}

impl Record {
    /// Becomes `r`, in the buffers it has; attribute pairs it has no more use for go to `spare`, and
    /// those it needs more of come from there. Records set over and over, whatever their
    /// attributes, allocate nothing once their buffers and the spares have grown.
    pub fn set_from(&mut self, r: &RecordRef<'_>, spare: &mut Vec<(String, String)>) {
        fn set(s: &mut String, v: &str) {
            s.clear();
            s.push_str(v);
        }
        self.ts = r.ts;
        self.severity = r.severity;
        set(&mut self.service, r.service);
        set(&mut self.host, r.host);
        set(&mut self.source, r.source);
        set(&mut self.message, r.message);
        let mut n = 0;
        for (k, v) in r.attrs() {
            match self.attrs.get_mut(n) {
                Some((a, b)) => {
                    set(a, k);
                    set(b, v);
                }
                None => match spare.pop() {
                    Some((mut a, mut b)) => {
                        set(&mut a, k);
                        set(&mut b, v);
                        self.attrs.push((a, b));
                    }
                    None => self.attrs.push((k.to_owned(), v.to_owned())),
                },
            }
            n += 1;
        }
        while self.attrs.len() > n {
            if let Some(pair) = self.attrs.pop() {
                spare.push(pair);
            }
        }
        self.trace_id = r.trace_id;
        self.span_id = r.span_id;
    }
}

/// The level a severity number names (OpenTelemetry's ranges: 1–4 trace, 5–8 debug, 9–12 info,
/// 13–16 warn, 17–20 error, 21–24 fatal).
pub fn level(severity: u8) -> &'static str {
    match severity {
        1..=4 => "trace",
        5..=8 => "debug",
        9..=12 => "info",
        13..=16 => "warn",
        17..=20 => "error",
        21..=24 => "fatal",
        _ => "unspecified",
    }
}

/// The severity numbers a level (or Datadog's and syslog's names for it) covers.
pub fn severities(level: &str) -> Option<std::ops::RangeInclusive<u8>> {
    Some(match level.to_ascii_lowercase().as_str() {
        "trace" => 1..=4,
        "debug" => 5..=8,
        "info" | "information" | "notice" | "ok" => 9..=12,
        "warn" | "warning" => 13..=16,
        "error" | "err" => 17..=20,
        "fatal" | "critical" | "crit" | "alert" | "emergency" | "emerg" => 21..=24,
        "unspecified" => 0..=0,
        _ => return None,
    })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    varint::put(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str<'a>(at: &mut &'a [u8]) -> Option<&'a str> {
    let n = usize::try_from(varint::get(at)?).ok()?;
    let (b, rest) = at.split_at_checked(n)?;
    *at = rest;
    std::str::from_utf8(b).ok()
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut c = max;
    while !s.is_char_boundary(c) {
        c -= 1;
    }
    s.get(..c).unwrap_or("")
}

/// Appends `r`'s row to `out`; `prev_ts` is the row before's time (0 for the first). Its
/// attributes' values are cut at a tag's length ([`crate::batch::MAX_NAME_BYTES`]), as a log's are,
/// but a stack's ([`crate::errors::STACK_KEYS`]: its ends kept, [`crate::errors::STACK_VALUE_BYTES`]).
pub fn put_row(out: &mut Vec<u8>, r: &Record, prev_ts: i64) {
    put_fields(out, r.ts, r.severity, [&r.service, &r.host, &r.source], &r.message, r.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())), r.attrs.len(), r.trace_id, r.span_id, prev_ts, crate::batch::MAX_NAME_BYTES);
}

/// Appends a row read in place, without copying it out first: as it is (a row's values were cut,
/// if at all, as it was first written; a span's are longer than a log's).
pub fn put_row_ref(out: &mut Vec<u8>, r: &RecordRef<'_>, prev_ts: i64) {
    put_fields(out, r.ts, r.severity, [r.service, r.host, r.source], r.message, r.attrs(), r.attr_count, r.trace_id, r.span_id, prev_ts, usize::MAX);
}

/// Appends a row from its fields, for a writer that makes no [`Record`] of it (a span decoded,
/// `crate::otlp::traces`): attributes' values longer than `value_max` are cut.
#[allow(clippy::too_many_arguments)]
pub fn put_fields<'a>(out: &mut Vec<u8>, ts: i64, severity: u8, names: [&str; 3], message: &str, attrs: impl Iterator<Item = (&'a str, &'a str)>, attr_count: usize, trace_id: Option<[u8; 16]>, span_id: Option<[u8; 8]>, prev_ts: i64, value_max: usize) {
    varint::put(out, varint::zig(ts.wrapping_sub(prev_ts)));
    out.push(severity);
    for s in names {
        put_str(out, cut(s, crate::batch::MAX_NAME_BYTES));
    }
    put_str(out, cut(message, MAX_MESSAGE_BYTES));
    let n = attr_count.min(crate::batch::MAX_TAGS);
    varint::put(out, n as u64);
    for (k, v) in attrs.take(n) {
        put_str(out, cut(k, crate::batch::MAX_NAME_BYTES));
        if crate::errors::STACK_KEYS.contains(&k) {
            // A stack: longer than another value, its ends kept (`crate::errors::stack_ends`).
            let (head, tail) = crate::errors::stack_ends(v, value_max.max(crate::errors::STACK_VALUE_BYTES));
            if tail.is_empty() {
                put_str(out, head);
            } else {
                varint::put(out, (head.len() + crate::errors::STACK_GAP.len() + tail.len()) as u64);
                out.extend_from_slice(head.as_bytes());
                out.extend_from_slice(crate::errors::STACK_GAP.as_bytes());
                out.extend_from_slice(tail.as_bytes());
            }
        } else {
            put_str(out, cut(v, value_max));
        }
    }
    out.push(u8::from(trace_id.is_some()) | (u8::from(span_id.is_some()) << 1));
    if let Some(t) = trace_id {
        out.extend_from_slice(&t);
    }
    if let Some(s) = span_id {
        out.extend_from_slice(&s);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a log row is malformed")]
pub struct RowError;

/// Reads rows in place.
#[derive(Debug, Clone)]
pub struct Rows<'a> {
    at: &'a [u8],
    prev_ts: i64,
    left: usize,
}

impl<'a> Rows<'a> {
    pub fn new(data: &'a [u8], rows: usize) -> Self {
        Self { at: data, prev_ts: 0, left: rows }
    }

    pub fn next_row(&mut self) -> Result<Option<RecordRef<'a>>, RowError> {
        if self.left == 0 {
            return Ok(None);
        }
        let r = self.read().ok_or(RowError);
        if r.is_err() {
            self.left = 0;
        }
        let r = r?;
        self.left -= 1;
        Ok(Some(r))
    }

    fn read(&mut self) -> Option<RecordRef<'a>> {
        let at = &mut self.at;
        let ts = self.prev_ts.wrapping_add(varint::zag(varint::get(at)?));
        let (&severity, rest) = at.split_first()?;
        *at = rest;
        let service = get_str(at)?;
        let host = get_str(at)?;
        let source = get_str(at)?;
        let message = get_str(at)?;
        let attr_count = usize::try_from(varint::get(at)?).ok()?;
        if attr_count > crate::batch::MAX_TAGS {
            return None;
        }
        let attrs_start = *at;
        for _ in 0..attr_count {
            get_str(at)?;
            get_str(at)?;
        }
        let attrs_raw = attrs_start.get(..attrs_start.len() - at.len())?;
        let (&flags, rest) = at.split_first()?;
        *at = rest;
        let trace_id = if flags & 1 != 0 {
            let (t, rest) = at.split_first_chunk::<16>()?;
            *at = rest;
            Some(*t)
        } else {
            None
        };
        let span_id = if flags & 2 != 0 {
            let (s, rest) = at.split_first_chunk::<8>()?;
            *at = rest;
            Some(*s)
        } else {
            None
        };
        self.prev_ts = ts;
        Some(RecordRef { ts, severity, service, host, source, message, attrs_raw, attr_count, trace_id, span_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(i: i64) -> Record {
        Record {
            ts: 1_700_000_000_000_000_000 + i * 1_000_000,
            severity: [9u8, 13, 17][(i % 3) as usize],
            service: ["api", "web"][(i % 2) as usize].into(),
            host: format!("h{}", i % 5),
            source: "kiln".into(),
            message: format!("request {i} took {} ms", i * 7 % 1000),
            attrs: vec![("http.status_code".into(), ["200", "404", "503"][(i % 3) as usize].into()), ("route".into(), "/api/runs".into())],
            trace_id: (i % 2 == 0).then_some([i as u8; 16]),
            span_id: (i % 4 == 0).then_some([7u8; 8]),
        }
    }

    #[test]
    fn rows_round_trip() {
        let recs: Vec<Record> = (0..200).map(sample).collect();
        let mut buf = Vec::new();
        let mut prev = 0;
        for r in &recs {
            put_row(&mut buf, r, prev);
            prev = r.ts;
        }
        let mut rows = Rows::new(&buf, recs.len());
        for r in &recs {
            let got = rows.next_row().unwrap().unwrap();
            assert_eq!(&got.to_owned(), r);
        }
        assert!(rows.next_row().unwrap().is_none());
        assert_eq!(level(17), "error");
        assert_eq!(severities("WARNING"), Some(13..=16));
    }

    #[test]
    fn malformed_rows_are_errors_never_panics() {
        let recs: Vec<Record> = (0..50).map(sample).collect();
        let mut buf = Vec::new();
        let mut prev = 0;
        for r in &recs {
            put_row(&mut buf, r, prev);
            prev = r.ts;
        }
        let mut rng = crate::testkit::Rng::new(4);
        for _ in 0..5000 {
            let mut bad = buf.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] = rng.next_u64() as u8;
            let cut = rng.below(bad.len() as u64) as usize;
            let mut rows = Rows::new(&bad[..cut], 60);
            while let Ok(Some(r)) = rows.next_row() {
                let _ = r.attrs().count();
            }
        }
    }
}
