//! Log lines as records (`docs/observe.md` §12.8): kiln's own logs, the runner's journal and every
//! pod's in the cluster, read from the files `KOBS_LOGS` names (`<journal>:<pods>`).
#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing, clippy::cast_precision_loss)]

use std::io::Read;

use kiln_obs::logs::record::Record;

/// Days from 1970-01-01 to a civil date (Hinnant, "chrono-Compatible Low-Level Date Algorithms").
pub fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-17T15:03:59.435765226Z`, `2026-10-17T17:00:19-0700` or `2026-10-16T19:52:49-07:00`
/// (journalctl's `short-iso`) at the line's start: ns.
pub fn leading_time(line: &str) -> Option<(i64, usize)> {
    let b = line.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| line.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut at = 19;
    let mut frac_ns = 0i64;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        let digits = &line[start..at.min(start + 9)];
        frac_ns = digits.parse::<i64>().unwrap_or(0) * 10i64.pow(9 - digits.len() as u32);
    }
    let mut off = 0i64;
    match b.get(at) {
        Some(b'Z') => at += 1,
        Some(&c) if c == b'+' || c == b'-' => {
            let colon = usize::from(b.get(at + 3) == Some(&b':'));
            let hh = n(at + 1..at + 3)?;
            let mm = n(at + 3 + colon..at + 5 + colon)?;
            off = (hh * 3600 + mm * 60) * if c == b'+' { 1 } else { -1 };
            at += 5 + colon;
        }
        _ => {}
    }
    let secs = days(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s - off;
    Some((secs * 1_000_000_000 + frac_ns, at))
}

pub fn severity(text: &str) -> u8 {
    let l = text.get(..text.len().min(200)).unwrap_or(text);
    if l.contains("level=error") || l.contains(" ERROR ") || l.contains("\"level\":\"error\"") || l.starts_with('E') && l.get(1..5).is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit())) {
        17
    } else if l.contains("level=warn") || l.contains(" WARN ") || l.contains("\"level\":\"warn") || l.starts_with('W') && l.get(1..5).is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit())) {
        13
    } else if l.contains("level=debug") || l.contains(" DEBUG ") {
        5
    } else {
        9
    }
}

pub fn to_record(line: &str, file: usize) -> Option<Record> {
    let (ts, at) = leading_time(line)?;
    let rest = line.get(at..)?.trim_start();
    let (service, host, message) = if file == 0 {
        // Journal: `host unit[pid]: message`.
        let mut parts = rest.splitn(3, ' ');
        let host = parts.next()?.to_owned();
        let unit = parts.next()?.split('[').next()?.trim_end_matches(':').to_owned();
        (unit, host, parts.next().unwrap_or("").to_owned())
    } else {
        let service = if rest.starts_with("time=") || rest.contains(" level=") {
            "argocd"
        } else if rest.starts_with('{') {
            "json-app"
        } else if rest.len() > 5 && matches!(rest.as_bytes()[0], b'I' | b'W' | b'E') && rest.as_bytes()[1].is_ascii_digit() {
            "kube"
        } else {
            "pods"
        };
        (service.to_owned(), "kiln-m02".to_owned(), rest.to_owned())
    };
    // logfmt pairs as attributes, at most 16.
    let mut attrs = Vec::new();
    for tok in message.split(' ').filter(|t| t.contains('=') && !t.starts_with('"')).take(16) {
        if let Some((k, v)) = tok.split_once('=')
            && !k.is_empty()
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            attrs.push((k.to_owned(), v.trim_matches('"').to_owned()));
        }
    }
    Some(Record { ts, severity: severity(&message), service, host, source: "corpus".into(), message, attrs, trace_id: None, span_id: None })
}

/// The records of the files `KOBS_LOGS` names, in time order, and the bytes of their lines.
pub fn load() -> Result<(Vec<Record>, usize), String> {
    let paths = std::env::var("KOBS_LOGS").map_err(|_| "set KOBS_LOGS to files of log lines (docs/observe.md §12.8)".to_owned())?;
    let mut records = Vec::new();
    let mut raw_bytes = 0usize;
    for (fi, p) in paths.split(':').enumerate() {
        let mut text = String::new();
        std::fs::File::open(p).and_then(|mut f| f.read_to_string(&mut text)).map_err(|e| format!("{p}: {e}"))?;
        for line in text.lines() {
            if let Some(r) = to_record(line, fi.min(1)) {
                raw_bytes += line.len() + 1;
                records.push(r);
            }
        }
    }
    records.sort_by_key(|r| r.ts);
    Ok((records, raw_bytes))
}
