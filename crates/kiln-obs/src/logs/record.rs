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

/// The names of levels (Datadog's and syslog's among them) and the severity numbers each covers.
const LEVEL_NAMES: [(&str, u8, u8); 17] = [
    ("trace", 1, 4),
    ("debug", 5, 8),
    ("info", 9, 12),
    ("information", 9, 12),
    ("notice", 9, 12),
    ("ok", 9, 12),
    ("warn", 13, 16),
    ("warning", 13, 16),
    ("error", 17, 20),
    ("err", 17, 20),
    ("fatal", 21, 24),
    ("critical", 21, 24),
    ("crit", 21, 24),
    ("alert", 21, 24),
    ("emergency", 21, 24),
    ("emerg", 21, 24),
    ("unspecified", 0, 0),
];

/// The severity numbers a level (or Datadog's and syslog's names for it) covers, its case
/// ignored (nothing allocated: a JSON line's level is read a line at a time).
pub fn severities(level: &str) -> Option<std::ops::RangeInclusive<u8>> {
    LEVEL_NAMES.iter().find(|(name, ..)| name.eq_ignore_ascii_case(level)).map(|&(_, a, b)| a..=b)
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

/// `s`, at most `max` bytes of it, cut at a character's start: what a row keeps of a text.
pub fn cut(s: &str, max: usize) -> &str {
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

/// Log records on their way to a write, in the order they came: a run of records (those a pipeline
/// reads, or that a collector made records of), or rows written whole (their times chained from 0),
/// how many, and the last one's time.
#[derive(Debug)]
pub enum LogPart {
    Records(Vec<Record>),
    Rows { raw: Vec<u8>, rows: u32, last_ts: i64 },
}

impl LogPart {
    /// Its records, none of rows.
    pub fn records(&self) -> &[Record] {
        match self {
            Self::Records(r) => r,
            Self::Rows { .. } => &[],
        }
    }

    pub fn records_mut(&mut self) -> &mut [Record] {
        match self {
            Self::Records(r) => r,
            Self::Rows { .. } => &mut [],
        }
    }

    /// Records, whichever form.
    pub fn len(&self) -> u64 {
        match self {
            Self::Records(r) => r.len() as u64,
            Self::Rows { rows, .. } => u64::from(*rows),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Appends rows written as a run of their own (their times chained from 0) after a row of time
/// `prev_ts`, as one run: the first's time chained from it, the rest as they are. `false`, nothing
/// appended, if the first row's time does not read.
pub fn put_rows_after(out: &mut Vec<u8>, raw: &[u8], prev_ts: i64) -> bool {
    if raw.is_empty() {
        return true;
    }
    let mut at = raw;
    let Some(delta) = varint::get(&mut at) else { return false };
    varint::put(out, varint::zig(varint::zag(delta).wrapping_sub(prev_ts)));
    out.extend_from_slice(at);
    true
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

    /// Runs written alone and joined, each first time chained again, read as the one run written
    /// whole: whatever the times (later, earlier, far apart), and a run of nothing joins as nothing.
    #[test]
    fn runs_joined_read_as_one_run() {
        let mut rng = crate::testkit::Rng::new(17);
        for _ in 0..200 {
            let runs: Vec<Vec<Record>> = (0..1 + rng.below(5))
                .map(|_| {
                    (0..rng.below(4))
                        .map(|_| Record { ts: [i64::MIN, -1, 0, 1, 1_700_000_000_000_000_000, i64::MAX][rng.below(6) as usize].wrapping_add(rng.below(1_000) as i64), ..sample(rng.below(100) as i64) })
                        .collect()
                })
                .collect();
            let (mut whole, mut joined, mut prev, mut last) = (Vec::new(), Vec::new(), 0, 0);
            for run in &runs {
                let mut alone = Vec::new();
                let mut p = 0;
                for r in run {
                    put_row(&mut whole, r, prev);
                    prev = r.ts;
                    put_row(&mut alone, r, p);
                    p = r.ts;
                }
                assert!(put_rows_after(&mut joined, &alone, last));
                if !run.is_empty() {
                    last = p;
                }
            }
            assert_eq!(joined, whole);
            let all: Vec<&Record> = runs.iter().flatten().collect();
            let mut rows = Rows::new(&joined, all.len());
            for r in all {
                assert_eq!(&rows.next_row().unwrap().unwrap().to_owned(), r);
            }
        }
        assert!(!put_rows_after(&mut Vec::new(), &[0x80], 5), "a time that does not read");
    }

    /// A level's name read whatever its case, as lowercasing it first reads it; anything else none.
    #[test]
    fn levels_are_read_whatever_their_case() {
        for &(name, a, b) in &LEVEL_NAMES {
            for spelled in [name.to_owned(), name.to_ascii_uppercase(), name.chars().enumerate().map(|(i, c)| if i % 2 == 0 { c.to_ascii_uppercase() } else { c }).collect()] {
                assert_eq!(severities(&spelled), Some(a..=b), "{spelled}");
            }
        }
        for none in ["", "inf", "infos", "warnings", " info", "info ", "İnfo", "debug\0", "erreur"] {
            assert_eq!(severities(none), None, "{none:?}");
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
