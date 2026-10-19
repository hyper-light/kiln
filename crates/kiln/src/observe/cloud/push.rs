//! The clouds' pushes (`docs/observe.md` §10.2): what a cloud sends kiln, where a pull would ask.
//!
//! - Amazon Kinesis Data Firehose's HTTP endpoint delivery (`POST /v1/firehose`), its access key
//!   observe's ingest token: CloudWatch Metric Streams in their JSON output format or their
//!   OpenTelemetry formats (1.0.0 and 0.7.0), named as the pulled metrics are (`aws.<namespace>.
//!   <metric>` with `.sum`, `.samplecount`, `.maximum`, `.minimum` and percentiles, the average as
//!   the name itself), and CloudWatch Logs' subscriptions, as the pulled logs are. Firehose's common
//!   attributes are tags of every series and record.
//! - Google Cloud Pub/Sub's push subscriptions (`POST /v1/pubsub?token=<the ingest token>`): a
//!   Cloud Logging sink's entries, as the pulled logs are.
//!
//! Both deliver at least once: a Firehose request, a Pub/Sub message and a log record taken are
//! kept for two hours (Firehose's longest retry), and one sent again is answered as taken and not
//! taken again. What observe refuses (it is busy) is answered so (503), and sent again.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use bytes::Bytes;
use http::StatusCode;
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use kiln_obs::otlp::pb::{Fields, PbError};
use serde_json::{Value, json};

use super::{Seen, aws, gcp, id_of, key};
use crate::http::server as srv;

/// How long a request, a message or a record taken is kept, to know it when it is sent again:
/// Firehose retries a delivery for up to two hours (its retry duration, 0 to 7,200 s).
const KEEP_MS: i64 = 2 * 3_600_000;
/// A delivery's body at the most, as sent and unpacked: Firehose's largest buffer for an HTTP
/// endpoint is 64 MiB, its records base64 (a third more) and gzip-compressed, or not.
const BODY: usize = 96 << 20;
const UNPACKED: usize = 256 << 20;
/// A record's data unpacked at the most (Firehose's records are 1,000 KiB at the most; a gzip'd
/// one unfolds some tenfold).
const RECORD: usize = 16 << 20;
/// A Pub/Sub message at the most (its limit is 10 MB).
const MESSAGE: usize = 12 << 20;

/// What the pushes took, kept to know a delivery sent again.
fn taken() -> &'static Mutex<Seen> {
    static TAKEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    TAKEN.get_or_init(Mutex::default)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a delivery held, ready to write.
#[derive(Debug, Default)]
pub struct Unpacked {
    pub series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    pub logs: Vec<(u64, Record)>,
    /// Records of no shape kiln reads (a log's message alone, a malformed one).
    pub skipped: u64,
}

impl Unpacked {
    pub fn points(&self) -> u64 {
        self.series.iter().map(|(_, p)| p.len() as u64).sum()
    }
}

fn gunzip(data: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data).take(max as u64 + 1).read_to_end(&mut out).map_err(|e| format!("not gzip: {e}"))?;
    if out.len() > max {
        return Err(format!("unpacked past {max} bytes"));
    }
    Ok(out)
}

/// Series by fingerprint, their points, as a delivery's records add them.
type Points = BTreeMap<u64, (SeriesKey, Vec<(i64, f64)>)>;

/// A metric's tags as a stream's datum gives them: its region and account, its dimensions by their
/// names in lower case, then Firehose's common attributes.
fn stream_tags(region: Option<&str>, account: Option<&str>, dimensions: impl Iterator<Item = (String, String)>, tags: &[(String, String)]) -> Vec<(String, String)> {
    let mut own: Vec<(String, String)> = Vec::new();
    for (k, x) in [("region", region), ("aws_account", account)] {
        if let Some(x) = x.filter(|x| !x.is_empty()) {
            own.push((k.to_owned(), x.to_owned()));
        }
    }
    own.extend(dimensions.map(|(k, x)| (k.to_ascii_lowercase(), x)));
    own.extend(tags.iter().cloned());
    own
}

/// A period's statistics as series (the pulls' names): `.sum` and `.samplecount` counts, the
/// average `sum / count` the name itself, the rest (`stats`: each one's suffix, `maximum`,
/// `minimum`, `p99`) gauges.
fn stream_stats(name: &str, own: &[(String, String)], ts: i64, sum_count: Option<(f64, f64)>, stats: &[(String, f64)], out: &mut Points) {
    let pairs: Vec<(&str, &str)> = own.iter().map(|(k, x)| (k.as_str(), x.as_str())).collect();
    let mut put = |metric: &str, kind: Kind, x: f64| {
        if x.is_finite()
            && let Some(k) = key(metric, &pairs, kind)
        {
            out.entry(k.fp).or_insert_with(|| (k, Vec::new())).1.push((ts, x));
        }
    };
    if let Some((sum, count)) = sum_count {
        put(&format!("{name}.sum"), Kind::Count, sum);
        put(&format!("{name}.samplecount"), Kind::Count, count);
        if count > 0.0 {
            put(name, Kind::Gauge, sum / count);
        }
    }
    for (suffix, x) in stats {
        put(&format!("{name}.{suffix}"), Kind::Gauge, *x);
    }
}

/// A Metric Streams record's statistics as series (the pulls' names and tags): `.sum` and
/// `.samplecount` counts, the average `sum / count` the name itself, the rest gauges.
fn stream_points(v: &Value, tags: &[(String, String)], out: &mut Points) -> bool {
    let (Some(ns), Some(metric), Some(ts), Some(value)) = (v.get("namespace").and_then(Value::as_str), v.get("metric_name").and_then(Value::as_str), v.get("timestamp").and_then(Value::as_i64), v.get("value").and_then(Value::as_object)) else {
        return false;
    };
    let dimensions = v.get("dimensions").and_then(Value::as_object).into_iter().flatten().filter_map(|(k, x)| Some((k.clone(), x.as_str()?.to_owned())));
    let own = stream_tags(v.get("region").and_then(Value::as_str), v.get("account_id").and_then(Value::as_str), dimensions, tags);
    let stat = |k: &str| value.get(k).and_then(Value::as_f64);
    let stats: Vec<(String, f64)> = value
        .iter()
        .filter_map(|(k, x)| {
            let suffix = match k.as_str() {
                "sum" | "count" => return None,
                "max" => "maximum".to_owned(),
                "min" => "minimum".to_owned(),
                other => aws::suffix(other),
            };
            Some((suffix, x.as_f64()?))
        })
        .collect();
    stream_stats(&aws::metric_name(ns, metric), &own, ts, stat("sum").zip(stat("count")), &stats, out);
    true
}

/// A Metric Streams record in OpenTelemetry's formats (1.0.0 and 0.7.0): one or more
/// `ExportMetricsServiceRequest`s, each after its length (an unsigned varint). `None` when the
/// record is not one: its frames do not cover it exactly, or one does not start as a request does,
/// with its resources (field 1, length-delimited: `0x0A`), as JSON's `{"` never does.
fn otel_frames(raw: &[u8]) -> Option<Vec<&[u8]>> {
    let mut at = raw;
    let mut frames = Vec::new();
    while !at.is_empty() {
        let n = usize::try_from(kiln_obs::varint::get(&mut at)?).ok()?;
        let (frame, rest) = at.split_at_checked(n)?;
        if frame.first() != Some(&0x0A) {
            return None;
        }
        frames.push(frame);
        at = rest;
    }
    (!frames.is_empty()).then_some(frames)
}

/// A `KeyValue` (1.0.0's attributes, and a resource's): its key and its `AnyValue`'s bytes.
fn key_value(kv: &[u8]) -> Result<(&str, &[u8]), PbError> {
    let (mut k, mut v) = ("", &[][..]);
    for f in Fields::new(kv) {
        match f? {
            (1, w) => k = w.str().unwrap_or(""),
            (2, w) => v = w.bytes().unwrap_or(&[]),
            _ => {}
        }
    }
    Ok((k, v))
}

/// An `AnyValue`'s string (`string_value`), if it holds one.
fn any_str(v: &[u8]) -> Result<Option<&str>, PbError> {
    for f in Fields::new(v) {
        if let (1, w) = f? {
            return Ok(w.str());
        }
    }
    Ok(None)
}

/// A quantile's statistic, as CloudWatch names it: 0 `minimum`, 1 `maximum`, 0.99 `p99`, 0.999
/// `p99_9` (`p99.9`'s suffix).
fn quantile_stat(q: f64) -> Option<String> {
    if q == 0.0 {
        return Some("minimum".into());
    }
    if q == 1.0 {
        return Some("maximum".into());
    }
    if !(q > 0.0 && q < 1.0) {
        return None;
    }
    // A percentile with six places at the most, its trailing zeros dropped: 0.999 × 100 is
    // 99.89999999999999 in binary, CloudWatch's `p99.9`.
    let mut p = format!("{:.6}", q * 100.0);
    while p.ends_with('0') {
        p.pop();
    }
    if p.ends_with('.') {
        p.pop();
    }
    Some(aws::suffix(&format!("p{p}")))
}

/// One `ExportMetricsServiceRequest` of a Metric Stream in OpenTelemetry's format, as its JSON is
/// read ([`stream_points`]): each data point a period's summary (its `count` the `SampleCount`, its
/// `sum` the `Sum`, its quantiles 0 and 1 the `Minimum` and `Maximum`, the rest percentiles; from
/// its start, CloudWatch's period's), named by its `Namespace` and `MetricName` and tagged by its
/// `Dimensions` (1.0.0: attributes, the dimensions a map; 0.7.0: labels, the dimensions the rest),
/// with its resource's `cloud.region` and `cloud.account.id`. Whether it held a point.
fn stream_otel(frame: &[u8], tags: &[(String, String)], out: &mut Points) -> Result<bool, PbError> {
    let mut read = false;
    for f in Fields::new(frame) {
        let (1, w) = f? else { continue };
        let (mut region, mut account, mut scopes) = (None, None, Vec::new());
        for f in Fields::new(w.bytes().unwrap_or(&[])) {
            match f? {
                (1, w) => {
                    for f in Fields::new(w.bytes().unwrap_or(&[])) {
                        let (1, w) = f? else { continue };
                        let (k, v) = key_value(w.bytes().unwrap_or(&[]))?;
                        match k {
                            "cloud.region" => region = any_str(v)?,
                            "cloud.account.id" => account = any_str(v)?,
                            _ => {}
                        }
                    }
                }
                (2, w) => scopes.push(w.bytes().unwrap_or(&[])),
                _ => {}
            }
        }
        for scope in scopes {
            for f in Fields::new(scope) {
                let (2, w) = f? else { continue };
                let (mut metric, mut summary) = ("", None);
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => metric = w.str().unwrap_or(""),
                        (11, w) => summary = w.bytes(),
                        _ => {}
                    }
                }
                for f in Fields::new(summary.unwrap_or(&[])) {
                    let (1, w) = f? else { continue };
                    read |= stream_otel_point(w.bytes().unwrap_or(&[]), metric, region, account, tags, out)?;
                }
            }
        }
    }
    Ok(read)
}

/// A summary's data point ([`stream_otel`]): whether it was one of a stream's.
fn stream_otel_point(p: &[u8], metric: &str, region: Option<&str>, account: Option<&str>, tags: &[(String, String)], out: &mut Points) -> Result<bool, PbError> {
    let (mut ns, mut name) = (None, None);
    let mut dimensions: Vec<(String, String)> = Vec::new();
    let (mut start, mut end, mut count, mut sum) = (0u64, 0u64, 0u64, 0.0f64);
    let mut stats: Vec<(String, f64)> = Vec::new();
    for f in Fields::new(p) {
        match f? {
            // 0.7.0's labels: `StringKeyValue`s, the namespace, the name and each dimension.
            (1, w) => {
                let (mut k, mut v) = ("", "");
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => k = w.str().unwrap_or(""),
                        (2, w) => v = w.str().unwrap_or(""),
                        _ => {}
                    }
                }
                match k {
                    "Namespace" => ns = Some(v),
                    "MetricName" => name = Some(v),
                    "" => {}
                    _ => dimensions.push((k.to_owned(), v.to_owned())),
                }
            }
            (2, w) => start = w.u64().unwrap_or(0),
            (3, w) => end = w.u64().unwrap_or(0),
            (4, w) => count = w.u64().unwrap_or(0),
            (5, w) => sum = w.f64().unwrap_or(0.0),
            (6, w) => {
                let (mut q, mut v) = (0.0, f64::NAN);
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => q = w.f64().unwrap_or(f64::NAN),
                        (2, w) => v = w.f64().unwrap_or(f64::NAN),
                        _ => {}
                    }
                }
                if let Some(stat) = quantile_stat(q) {
                    stats.push((stat, v));
                }
            }
            // 1.0.0's attributes: the namespace, the name, and the dimensions as a map.
            (7, w) => {
                let (k, v) = key_value(w.bytes().unwrap_or(&[]))?;
                match k {
                    "Namespace" => ns = any_str(v)?,
                    "MetricName" => name = any_str(v)?,
                    "Dimensions" => {
                        for f in Fields::new(v) {
                            let (6, w) = f? else { continue };
                            for f in Fields::new(w.bytes().unwrap_or(&[])) {
                                let (1, w) = f? else { continue };
                                let (k, v) = key_value(w.bytes().unwrap_or(&[]))?;
                                if let (false, Some(v)) = (k.is_empty(), any_str(v)?) {
                                    dimensions.push((k.to_owned(), v.to_owned()));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Without its attributes, the metric's own name says them: `amazonaws.com/<namespace>/<name>`.
    let named = metric.strip_prefix("amazonaws.com/").and_then(|m| m.rsplit_once('/'));
    let (Some(ns), Some(name)) = (ns.or(named.map(|n| n.0)), name.or(named.map(|n| n.1))) else { return Ok(false) };
    let at = if start > 0 { start } else { end };
    if at == 0 || ns.is_empty() || name.is_empty() {
        return Ok(false);
    }
    let ts = i64::try_from(at / 1_000_000).unwrap_or(i64::MAX);
    let own = stream_tags(region, account, dimensions.into_iter(), tags);
    stream_stats(&aws::metric_name(ns, name), &own, ts, Some((sum, count as f64)), &stats, out);
    Ok(true)
}

/// A CloudWatch Logs subscription's message (`DATA_MESSAGE`) as records; a `CONTROL_MESSAGE` (its
/// check that the destination is there) as none. Whether it was one.
fn subscription_logs(v: &Value, region: &str, tags: &[(String, String)], out: &mut Vec<(u64, Record)>) -> bool {
    let Some(kind) = v.get("messageType").and_then(Value::as_str) else { return false };
    if kind != "DATA_MESSAGE" {
        return true;
    }
    let group = v.get("logGroup").and_then(Value::as_str).unwrap_or("");
    let stream = v.get("logStream").and_then(Value::as_str).unwrap_or("");
    let mut own: Vec<(String, String)> = tags.to_vec();
    if let Some(owner) = v.get("owner").and_then(Value::as_str) {
        own.push(("aws_account".into(), owner.to_owned()));
    }
    for e in v.get("logEvents").and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(ts)) = (e.get("id").and_then(Value::as_str), e.get("timestamp").and_then(Value::as_i64)) else { continue };
        let message = e.get("message").and_then(Value::as_str).unwrap_or("").to_owned();
        out.push(aws::record_of(region, group, aws::Event { event_id: id.to_owned(), log_stream_name: stream.to_owned(), timestamp: ts, message }, &own));
    }
    true
}

/// A Firehose delivery's records, each base64 and maybe gzip'd: Metric Streams' JSON lines, or a
/// CloudWatch Logs subscription's message. `region` is the delivery stream's.
pub fn unpack_firehose(records: &[Value], region: &str, tags: &[(String, String)]) -> Result<Unpacked, String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Unpacked::default();
    let mut points = Points::new();
    for r in records {
        let data = r.get("data").and_then(Value::as_str).ok_or("a record without data")?;
        let raw = b64.decode(data).map_err(|e| format!("a record's data is not base64: {e}"))?;
        let raw = if raw.starts_with(&[0x1f, 0x8b]) { gunzip(&raw, RECORD)? } else { raw };
        // OpenTelemetry's format: each request's points kept only if all of it reads; the record
        // counted as not read if one did not, or none held a point.
        if let Some(frames) = otel_frames(&raw) {
            let (mut read, mut bad) = (false, false);
            for frame in frames {
                let mut mine = Points::new();
                match stream_otel(frame, tags, &mut mine) {
                    Ok(r) => {
                        read |= r;
                        for (fp, (k, p)) in mine {
                            points.entry(fp).or_insert_with(|| (k, Vec::new())).1.extend(p);
                        }
                    }
                    Err(_) => bad = true,
                }
            }
            if bad || !read {
                out.skipped += 1;
            }
            continue;
        }
        let text = String::from_utf8_lossy(&raw);
        let mut read = false;
        // Newline-delimited JSON (Metric Streams' JSON), or one object (a subscription's message).
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            read |= subscription_logs(&v, region, tags, &mut out.logs) || stream_points(&v, tags, &mut points);
        }
        if !read {
            out.skipped += 1;
        }
    }
    out.series = points.into_values().collect();
    Ok(out)
}

/// The region of a Firehose delivery stream's ARN (`arn:aws:firehose:<region>:<account>:…`).
fn region_of(arn: &str) -> &str {
    arn.split(':').nth(3).unwrap_or("")
}

/// Firehose's common attributes (`{"commonAttributes": {…}}`), as tags.
fn common_attributes(header: Option<&str>) -> Vec<(String, String)> {
    let Some(v) = header.and_then(|h| serde_json::from_str::<Value>(h).ok()) else { return Vec::new() };
    v.get("commonAttributes").and_then(Value::as_object).into_iter().flatten().filter_map(|(k, x)| Some((k.clone(), x.as_str()?.to_owned()))).collect()
}

/// Firehose's answer: its request's id and the time, and why not when it is not taken.
fn firehose_answer(code: StatusCode, request: &str, error: Option<&str>) -> srv::Response {
    let mut body = json!({ "requestId": request, "timestamp": super::super::clock_ms() });
    if let (Some(e), Some(o)) = (error, body.as_object_mut()) {
        o.insert("errorMessage".into(), e.into());
    }
    srv::json(code, &body)
}

/// Writes what a delivery held, its log records not taken before: whether observe took it.
fn write(u: Unpacked, seen: &mut Seen) -> Result<(u64, u64, u64), &'static str> {
    let o = super::super::api::get().ok_or("observe is starting")?;
    let points = u.points();
    if !u.series.is_empty() && !o.offer(u.series) {
        return Err("observe is busy: send it again");
    }
    let mut fresh: Vec<(u64, Record)> = u.logs.into_iter().filter(|(id, _)| !seen.has(*id)).collect();
    fresh.sort_by_key(|(id, _)| *id);
    fresh.dedup_by_key(|(id, _)| *id);
    let ids: Vec<(u64, i64)> = fresh.iter().map(|(id, r)| (*id, r.ts.div_euclid(1_000_000))).collect();
    let records = fresh.len() as u64;
    if !fresh.is_empty() && !o.offer_logs(fresh.into_iter().map(|(_, r)| r).collect()) {
        return Err("observe is busy: send it again");
    }
    for (id, ms) in ids {
        seen.add(id, ms.max(super::super::clock_ms()));
    }
    Ok((points, records, u.skipped))
}

/// How often kiln's own counts of the pushes are written: each push's count a point of its own
/// would keep one of two pushes at one millisecond (the store keeps one value a time), so they are
/// added up, and written as Prometheus's default scrape would read them.
const TALLY_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// The pushes' counts not yet written, by series name and `via`.
fn tally() -> &'static Mutex<BTreeMap<(&'static str, &'static str), f64>> {
    static TALLY: OnceLock<Mutex<BTreeMap<(&'static str, &'static str), f64>>> = OnceLock::new();
    TALLY.get_or_init(|| {
        crate::rt::spawn(async {
            loop {
                crate::rt::sleep(TALLY_EVERY).await;
                write_tally();
            }
        });
        Mutex::default()
    })
}

fn write_tally() {
    let Some(o) = super::super::api::get() else { return };
    let counted = std::mem::take(&mut *lock(tally()));
    let ts = super::super::clock_ms();
    let out: Vec<(SeriesKey, Vec<(i64, f64)>)> = counted.into_iter().filter_map(|((m, via), v)| Some((SeriesKey::new(m, [("via", via)], Kind::Count).ok()?, vec![(ts, v)]))).collect();
    if !out.is_empty() {
        o.write(out);
    }
}

/// kiln's own counts of a push: what it took, what was sent again, refused, or not read.
fn count(via: &'static str, points: u64, records: u64, again: u64, refused: u64, skipped: u64) {
    let mut t = lock(tally());
    for (m, v) in [("kiln.cloud.push.points", points), ("kiln.cloud.push.records", records), ("kiln.cloud.push.again", again), ("kiln.cloud.push.refused", refused), ("kiln.cloud.push.skipped", skipped)] {
        *t.entry((m, via)).or_default() += v as f64;
    }
}

/// `POST /v1/firehose`: a Kinesis Data Firehose delivery (`authorised`: its access key was the
/// ingest token or the admin's).
pub async fn firehose(req: srv::Request, authorised: bool) -> srv::Response {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let request = header("x-amz-firehose-request-id").unwrap_or_default();
    if !authorised {
        return firehose_answer(StatusCode::UNAUTHORIZED, &request, Some("the access key is not observe's ingest token"));
    }
    let gzip = header("content-encoding").is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
    let region = header("x-amz-firehose-source-arn").map(|a| region_of(&a).to_owned()).unwrap_or_default();
    let tags = common_attributes(header("x-amz-firehose-common-attributes").as_deref());
    let body: Bytes = match srv::body_bytes(req.into_body(), BODY).await {
        Ok(b) => b,
        Err(e) => return firehose_answer(StatusCode::PAYLOAD_TOO_LARGE, &request, Some(&format!("{e:#}"))),
    };
    let plain = if gzip {
        match gunzip(&body, UNPACKED) {
            Ok(p) => p,
            Err(e) => return firehose_answer(StatusCode::BAD_REQUEST, &request, Some(&e)),
        }
    } else {
        body.to_vec()
    };
    let delivery: Value = match serde_json::from_slice(&plain) {
        Ok(v) => v,
        Err(e) => return firehose_answer(StatusCode::BAD_REQUEST, &request, Some(&format!("not a Firehose delivery: {e}"))),
    };
    let request = delivery.get("requestId").and_then(Value::as_str).map_or(request, str::to_owned);
    let id = id_of(&["firehose", &request]);
    let records = delivery.get("records").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let unpacked = match unpack_firehose(records, &region, &tags) {
        Ok(u) => u,
        Err(e) => return firehose_answer(StatusCode::BAD_REQUEST, &request, Some(&e)),
    };
    let mut seen = lock(taken());
    if !request.is_empty() && seen.has(id) {
        drop(seen);
        count("firehose", 0, 0, 1, 0, 0);
        return firehose_answer(StatusCode::OK, &request, None);
    }
    match write(unpacked, &mut seen) {
        Ok((points, records, skipped)) => {
            let now = super::super::clock_ms();
            if !request.is_empty() {
                seen.add(id, now);
            }
            seen.prune(now.saturating_sub(KEEP_MS));
            drop(seen);
            count("firehose", points, records, 0, 0, skipped);
            firehose_answer(StatusCode::OK, &request, None)
        }
        Err(why) => {
            drop(seen);
            count("firehose", 0, 0, 0, 1, 0);
            firehose_answer(StatusCode::SERVICE_UNAVAILABLE, &request, Some(why))
        }
    }
}

/// A Pub/Sub push's message as records: a Cloud Logging entry (its project from its log's name).
pub fn unpack_pubsub(push: &Value) -> Result<(Option<String>, Unpacked), String> {
    let message = push.get("message").ok_or("not a Pub/Sub push: no message")?;
    let id = message.get("messageId").or_else(|| message.get("message_id")).and_then(Value::as_str).map(str::to_owned);
    let data = message.get("data").and_then(Value::as_str).unwrap_or("");
    let raw = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("the message's data is not base64: {e}"))?;
    let mut out = Unpacked::default();
    match serde_json::from_slice::<Value>(&raw) {
        Ok(entry) if entry.get("logName").is_some() => {
            let project = entry.get("logName").and_then(Value::as_str).and_then(|l| l.strip_prefix("projects/")).and_then(|l| l.split('/').next()).unwrap_or("").to_owned();
            match gcp::record_of(&project, &entry, &[]) {
                Some(r) => out.logs.push(r),
                None => out.skipped += 1,
            }
        }
        _ => out.skipped += 1,
    }
    Ok((id, out))
}

/// `POST /v1/pubsub?token=…`: a Pub/Sub push (`authorised`: its token was the ingest token or the
/// admin's). Answered 204 when taken (an acknowledgement); a message kiln cannot read is taken and
/// counted, not sent again forever.
pub async fn pubsub(req: srv::Request, authorised: bool) -> srv::Response {
    if !authorised {
        return srv::text(StatusCode::UNAUTHORIZED, "a push subscription's endpoint ends in ?token=<observe's ingest token>");
    }
    let body: Bytes = match srv::body_bytes(req.into_body(), MESSAGE).await {
        Ok(b) => b,
        Err(e) => return srv::text(StatusCode::PAYLOAD_TOO_LARGE, &format!("{e:#}")),
    };
    let push: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return srv::text(StatusCode::BAD_REQUEST, &format!("not a Pub/Sub push: {e}")),
    };
    let (message, unpacked) = match unpack_pubsub(&push) {
        Ok(x) => x,
        Err(e) => return srv::text(StatusCode::BAD_REQUEST, &e),
    };
    let subscription = push.get("subscription").and_then(Value::as_str).unwrap_or("");
    let id = message.as_deref().map(|m| id_of(&["pubsub", subscription, m]));
    let mut seen = lock(taken());
    if id.is_some_and(|i| seen.has(i)) {
        drop(seen);
        count("pubsub", 0, 0, 1, 0, 0);
        return srv::status(StatusCode::NO_CONTENT);
    }
    match write(unpacked, &mut seen) {
        Ok((points, records, skipped)) => {
            let now = super::super::clock_ms();
            if let Some(i) = id {
                seen.add(i, now);
            }
            seen.prune(now.saturating_sub(KEEP_MS));
            drop(seen);
            count("pubsub", points, records, 0, 0, skipped);
            srv::status(StatusCode::NO_CONTENT)
        }
        Err(why) => {
            drop(seen);
            count("pubsub", 0, 0, 0, 1, 0);
            srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, why), http::header::RETRY_AFTER, "1")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = e.write_all(data);
        e.finish().unwrap_or_default()
    }

    #[test]
    fn a_metric_stream_s_json_and_a_subscription_s_logs_are_what_the_pulls_make() -> anyhow::Result<()> {
        let lines = concat!(
            r#"{"metric_stream_name":"s","account_id":"123456789012","region":"us-east-1","namespace":"AWS/EC2","metric_name":"CPUUtilization","dimensions":{"InstanceId":"i-1"},"timestamp":1800000000000,"value":{"max":9.0,"min":1.0,"sum":20.0,"count":4.0,"p99":8.5},"unit":"Percent"}"#,
            "\n",
            r#"{"metric_stream_name":"s","account_id":"123456789012","region":"us-east-1","namespace":"AWS/Lambda","metric_name":"Invocations","dimensions":{"FunctionName":"checkout"},"timestamp":1800000060000,"value":{"max":1.0,"min":1.0,"sum":7.0,"count":7.0},"unit":"Count"}"#,
            "\n"
        );
        let logs = json!({ "messageType": "DATA_MESSAGE", "owner": "123456789012", "logGroup": "/aws/lambda/checkout", "logStream": "s1", "subscriptionFilters": ["all"], "logEvents": [{ "id": "1", "timestamp": 1_800_000_000_000i64, "message": "[ERROR] no" }, { "id": "2", "timestamp": 1_800_000_000_001i64, "message": "ok" }] });
        let control = json!({ "messageType": "CONTROL_MESSAGE", "owner": "CloudwatchLogs", "logGroup": "", "logStream": "", "subscriptionFilters": [], "logEvents": [{ "id": "", "timestamp": 1, "message": "CWL CONTROL MESSAGE: Checking health of destination Firehose." }] });
        let records = vec![json!({ "data": b64(lines.as_bytes()) }), json!({ "data": b64(&gz(logs.to_string().as_bytes())) }), json!({ "data": b64(&gz(control.to_string().as_bytes())) }), json!({ "data": b64(b"just a message") })];
        let u = unpack_firehose(&records, "us-east-1", &[("env".into(), "prod".into())]).map_err(anyhow::Error::msg)?;
        let names: Vec<String> = u.series.iter().filter_map(|(k, p)| kiln_obs::batch::parse_key(&k.key).ok().map(|k| format!("{} {:?} {p:?}", k.metric, k.tags))).collect();
        let has = |want: &str| names.iter().any(|n| n.starts_with(want));
        for want in ["aws.ec2.cpuutilization ", "aws.ec2.cpuutilization.sum ", "aws.ec2.cpuutilization.samplecount ", "aws.ec2.cpuutilization.maximum ", "aws.ec2.cpuutilization.minimum ", "aws.ec2.cpuutilization.p99 ", "aws.lambda.invocations.sum "] {
            anyhow::ensure!(has(want), "{want} in {names:#?}");
        }
        anyhow::ensure!(names.iter().any(|n| n.starts_with("aws.ec2.cpuutilization ") && n.contains("(\"instanceid\", \"i-1\")") && n.contains("(\"region\", \"us-east-1\")") && n.contains("(\"env\", \"prod\")") && n.ends_with("[(1800000000000, 5.0)]")), "the average, tagged: {names:#?}");
        anyhow::ensure!(u.logs.len() == 2 && u.skipped == 1, "{} records, {} skipped", u.logs.len(), u.skipped);
        let r = &u.logs.first().ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(r.source == "lambda" && r.service == "checkout" && r.severity == 17 && r.attrs.contains(&("aws_account".into(), "123456789012".into())) && r.attrs.contains(&("env".into(), "prod".into())), "{r:?}");
        anyhow::ensure!(region_of("arn:aws:firehose:eu-west-1:123456789012:deliverystream/kiln") == "eu-west-1");
        anyhow::ensure!(common_attributes(Some(r#"{"commonAttributes":{"team":"pay"}}"#)) == vec![("team".to_owned(), "pay".to_owned())]);
        Ok(())
    }

    /// A stream's datum in OpenTelemetry 1.0.0's format: its resource, a summary point of each
    /// period (`start_ms`, a minute), its quantiles as AWS writes them (0's quantile left out, as
    /// proto3 leaves out zeros).
    fn otel_100(points: &[(i64, u64, f64, &[(f64, f64)])], named: bool) -> Vec<u8> {
        use kiln_obs::otlp::pb::Writer;
        let mut w = Writer::default();
        w.message(1, |rm| {
            rm.message(1, |res| {
                for (k, v) in [("cloud.provider", "aws"), ("cloud.account.id", "123456789012"), ("cloud.region", "us-east-1"), ("aws.exporter.arn", "arn:aws:cloudwatch:us-east-1:123456789012:metric-stream/s")] {
                    res.message(1, |kv| {
                        kv.str(1, k);
                        kv.message(2, |av| av.str(1, v));
                    });
                }
            });
            rm.message(2, |sm| {
                sm.message(2, |m| {
                    m.str(1, "amazonaws.com/AWS/DynamoDB/ConsumedReadCapacityUnits");
                    m.str(3, "1");
                    m.message(11, |summary| {
                        for &(start_ms, count, sum, quantiles) in points {
                            summary.message(1, |dp| {
                                dp.fixed64(2, start_ms as u64 * 1_000_000);
                                dp.fixed64(3, (start_ms as u64 + 60_000) * 1_000_000);
                                dp.fixed64(4, count);
                                dp.double(5, sum);
                                for &(q, v) in quantiles {
                                    dp.message(6, |qv| {
                                        if q != 0.0 {
                                            qv.double(1, q);
                                        }
                                        qv.double(2, v);
                                    });
                                }
                                if named {
                                    for (k, v) in [("Namespace", "AWS/DynamoDB"), ("MetricName", "ConsumedReadCapacityUnits")] {
                                        dp.message(7, |kv| {
                                            kv.str(1, k);
                                            kv.message(2, |av| av.str(1, v));
                                        });
                                    }
                                }
                                dp.message(7, |kv| {
                                    kv.str(1, "Dimensions");
                                    kv.message(2, |av| {
                                        av.message(6, |map| {
                                            map.message(1, |d| {
                                                d.str(1, "TableName");
                                                d.message(2, |av| av.str(1, "MyTable"));
                                            });
                                        });
                                    });
                                });
                            });
                        }
                    });
                });
            });
        });
        w.buf
    }

    /// The same in 0.7.0's: the namespace, the name and the dimensions as labels.
    fn otel_070(points: &[(i64, u64, f64, &[(f64, f64)])]) -> Vec<u8> {
        use kiln_obs::otlp::pb::Writer;
        let mut w = Writer::default();
        w.message(1, |rm| {
            rm.message(1, |res| {
                for (k, v) in [("cloud.provider", "aws"), ("cloud.account.id", "123456789012"), ("cloud.region", "us-east-1")] {
                    res.message(1, |kv| {
                        kv.str(1, k);
                        kv.message(2, |av| av.str(1, v));
                    });
                }
            });
            rm.message(2, |ilm| {
                ilm.message(2, |m| {
                    m.str(1, "amazonaws.com/AWS/DynamoDB/ConsumedReadCapacityUnits");
                    m.message(11, |summary| {
                        for &(start_ms, count, sum, quantiles) in points {
                            summary.message(1, |dp| {
                                for (k, v) in [("Namespace", "AWS/DynamoDB"), ("MetricName", "ConsumedReadCapacityUnits"), ("TableName", "MyTable")] {
                                    dp.message(1, |l| {
                                        l.str(1, k);
                                        l.str(2, v);
                                    });
                                }
                                dp.fixed64(2, start_ms as u64 * 1_000_000);
                                dp.fixed64(3, (start_ms as u64 + 60_000) * 1_000_000);
                                dp.fixed64(4, count);
                                dp.double(5, sum);
                                for &(q, v) in quantiles {
                                    dp.message(6, |qv| {
                                        qv.double(1, q);
                                        qv.double(2, v);
                                    });
                                }
                            });
                        }
                    });
                });
            });
        });
        w.buf
    }

    /// Requests one after another, each after its length.
    fn delimited(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            kiln_obs::varint::put(&mut out, f.len() as u64);
            out.extend_from_slice(f);
        }
        out
    }

    fn series_of(u: &Unpacked) -> Vec<String> {
        let mut out: Vec<String> = u.series.iter().filter_map(|(s, p)| kiln_obs::batch::parse_key(&s.key).ok().map(|k| format!("{} {:?} {:?} {p:?}", k.metric, s.kind, k.tags))).collect();
        out.sort();
        out
    }

    /// A stream's datum is the same series whichever of its three formats it came in: JSON's,
    /// OpenTelemetry 1.0.0's (its quantiles, its dimensions a map), and 0.7.0's (labels).
    #[test]
    fn a_metric_stream_s_opentelemetry_formats_read_as_its_json_does() -> anyhow::Result<()> {
        let t = 1_800_000_000_000i64;
        let one: &[(f64, f64)] = &[(0.0, 1.0), (0.99, 2.5), (0.999, 2.9), (1.0, 3.0)];
        let two: &[(f64, f64)] = &[(0.0, 2.0), (1.0, 2.0)];
        let points = [(t, 2u64, 5.0, one), (t + 60_000, 1, 2.0, two)];
        let mut lines = String::new();
        for (ts, count, sum, value) in [(t, 2.0, 5.0, json!({ "min": 1.0, "p99": 2.5, "p99.9": 2.9, "max": 3.0 })), (t + 60_000, 1.0, 2.0, json!({ "min": 2.0, "max": 2.0 }))] {
            let mut value = value;
            if let Some(o) = value.as_object_mut() {
                o.insert("sum".into(), json!(sum));
                o.insert("count".into(), json!(count));
            }
            lines.push_str(&json!({ "metric_stream_name": "s", "account_id": "123456789012", "region": "us-east-1", "namespace": "AWS/DynamoDB", "metric_name": "ConsumedReadCapacityUnits", "dimensions": { "TableName": "MyTable" }, "timestamp": ts, "value": value, "unit": "None" }).to_string());
            lines.push('\n');
        }
        let tags = [("team".to_owned(), "pay".to_owned())];
        let json_way = unpack_firehose(&[json!({ "data": b64(lines.as_bytes()) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?;
        let want = series_of(&json_way);
        anyhow::ensure!(want.len() == 7 && json_way.skipped == 0, "{want:#?}");
        // 1.0.0: both periods in one request; then each in a request of its own, one record.
        let v100 = unpack_firehose(&[json!({ "data": b64(&delimited(&[otel_100(&points, true)])) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(series_of(&v100) == want && v100.skipped == 0, "{:#?}\nagainst {want:#?}", series_of(&v100));
        let split = delimited(&[otel_100(&points[..1], true), otel_100(&points[1..], true)]);
        anyhow::ensure!(series_of(&unpack_firehose(&[json!({ "data": b64(&split) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?) == want);
        // Without its attributes, named by its metric's name.
        anyhow::ensure!(series_of(&unpack_firehose(&[json!({ "data": b64(&delimited(&[otel_100(&points, false)])) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?) == want);
        let v070 = unpack_firehose(&[json!({ "data": b64(&delimited(&[otel_070(&points)])) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(series_of(&v070) == want && v070.skipped == 0, "{:#?}", series_of(&v070));
        // A request cut short: its record not read and counted so, the others' points kept.
        let whole = otel_100(&points[..1], true);
        let mut cut = delimited(std::slice::from_ref(&whole));
        cut.truncate(cut.len() - 3);
        let short = whole.get(..whole.len() - 3).unwrap_or(&[]);
        let mut lying = Vec::new();
        kiln_obs::varint::put(&mut lying, short.len() as u64);
        lying.extend_from_slice(short);
        let u = unpack_firehose(&[json!({ "data": b64(&cut) }), json!({ "data": b64(&lying) }), json!({ "data": b64(&delimited(&[otel_100(&points[1..], true)])) })], "us-east-1", &tags).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(u.skipped == 2 && u.points() == 5, "{} skipped, {} points: {:#?}", u.skipped, u.points(), series_of(&u));
        // JSON that happens to start as a length would (`{` is 123) is still JSON.
        anyhow::ensure!(otel_frames(lines.as_bytes()).is_none());
        anyhow::ensure!(quantile_stat(0.5).as_deref() == Some("p50") && quantile_stat(0.999).as_deref() == Some("p99_9") && quantile_stat(0.9999).as_deref() == Some("p99_99") && quantile_stat(1.5).is_none());
        Ok(())
    }

    #[test]
    fn a_pub_sub_push_is_a_cloud_logging_entry_s_record() -> anyhow::Result<()> {
        let entry = json!({ "logName": "projects/p1/logs/run.googleapis.com%2Fstdout", "insertId": "x", "timestamp": "2026-10-18T12:00:00Z", "severity": "WARNING", "textPayload": "slow", "resource": { "type": "cloud_run_revision", "labels": { "service_name": "web" } } });
        let push = json!({ "message": { "data": b64(entry.to_string().as_bytes()), "messageId": "m1", "publishTime": "2026-10-18T12:00:01Z" }, "subscription": "projects/p1/subscriptions/kiln" });
        let (id, u) = unpack_pubsub(&push).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(id.as_deref() == Some("m1") && u.logs.len() == 1 && u.skipped == 0);
        let r = &u.logs.first().ok_or_else(|| anyhow::anyhow!("no record"))?.1;
        anyhow::ensure!(r.source == "gcp.cloud_run_revision" && r.service == "web" && r.severity == 13 && r.message == "slow", "{r:?}");
        let other = json!({ "message": { "data": b64(b"{\"hello\": 1}"), "messageId": "m2" }, "subscription": "s" });
        anyhow::ensure!(unpack_pubsub(&other).map_err(anyhow::Error::msg)?.1.skipped == 1, "a message that is no log entry is counted, not read");
        anyhow::ensure!(unpack_pubsub(&json!({ "nope": 1 })).is_err());
        Ok(())
    }
}
