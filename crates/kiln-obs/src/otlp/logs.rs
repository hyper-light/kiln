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
//!   text, at most `MAX_TAGS`; the event's name, if any, is the attribute `event.name`.
//! - A trace id of 16 bytes and a span id of 8 are kept; others (empty: none) are not.

use super::metrics::{OtlpError, any_value_text_within, key_value};
use super::pb::Fields;
use crate::batch::MAX_TAGS;
use crate::logs::record::{MAX_MESSAGE_BYTES, Record, severities};

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
                        && let Some(t) = key_value(kv.bytes().unwrap_or(&[]))?
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
                if let Some(t) = key_value(w.bytes().unwrap_or(&[]))? {
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
