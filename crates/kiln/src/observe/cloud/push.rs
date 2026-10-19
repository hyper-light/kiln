//! The clouds' pushes (`docs/observe.md` §10.2): what a cloud sends kiln, where a pull would ask.
//!
//! - Amazon Kinesis Data Firehose's HTTP endpoint delivery (`POST /v1/firehose`), its access key
//!   observe's ingest token: CloudWatch Metric Streams in their JSON output format, named as the
//!   pulled metrics are (`aws.<namespace>.<metric>` with `.sum`, `.samplecount`, `.maximum`,
//!   `.minimum`, the average as the name itself), and CloudWatch Logs' subscriptions, as the pulled
//!   logs are. Firehose's common attributes are tags of every series and record.
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
    /// Records of no shape kiln reads (Metric Streams' OpenTelemetry format, a log's message alone).
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

/// A Metric Streams record's statistics as series (the pulls' names and tags): `.sum` and
/// `.samplecount` counts, the average `sum / count` the name itself, the rest gauges.
fn stream_points(v: &Value, tags: &[(String, String)], out: &mut BTreeMap<u64, (SeriesKey, Vec<(i64, f64)>)>) -> bool {
    let (Some(ns), Some(metric), Some(ts), Some(value)) = (v.get("namespace").and_then(Value::as_str), v.get("metric_name").and_then(Value::as_str), v.get("timestamp").and_then(Value::as_i64), v.get("value").and_then(Value::as_object)) else {
        return false;
    };
    let name = aws::metric_name(ns, metric);
    let mut own: Vec<(String, String)> = Vec::new();
    for (k, field) in [("region", "region"), ("aws_account", "account_id")] {
        if let Some(x) = v.get(field).and_then(Value::as_str).filter(|x| !x.is_empty()) {
            own.push((k.to_owned(), x.to_owned()));
        }
    }
    for (k, x) in v.get("dimensions").and_then(Value::as_object).into_iter().flatten() {
        if let Some(x) = x.as_str() {
            own.push((k.to_ascii_lowercase(), x.to_owned()));
        }
    }
    own.extend(tags.iter().cloned());
    let pairs: Vec<(&str, &str)> = own.iter().map(|(k, x)| (k.as_str(), x.as_str())).collect();
    let mut put = |metric: String, kind: Kind, x: f64| {
        if x.is_finite()
            && let Some(k) = key(&metric, &pairs, kind)
        {
            out.entry(k.fp).or_insert_with(|| (k, Vec::new())).1.push((ts, x));
        }
    };
    let stat = |k: &str| value.get(k).and_then(Value::as_f64);
    if let (Some(sum), Some(count)) = (stat("sum"), stat("count")) {
        put(format!("{name}.sum"), Kind::Count, sum);
        put(format!("{name}.samplecount"), Kind::Count, count);
        if count > 0.0 {
            put(name.clone(), Kind::Gauge, sum / count);
        }
    }
    for (k, x) in value {
        let Some(x) = x.as_f64() else { continue };
        match k.as_str() {
            "sum" | "count" => {}
            "max" => put(format!("{name}.maximum"), Kind::Gauge, x),
            "min" => put(format!("{name}.minimum"), Kind::Gauge, x),
            other => put(format!("{name}.{}", aws::suffix(other)), Kind::Gauge, x),
        }
    }
    true
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
    let mut points: BTreeMap<u64, (SeriesKey, Vec<(i64, f64)>)> = BTreeMap::new();
    for r in records {
        let data = r.get("data").and_then(Value::as_str).ok_or("a record without data")?;
        let raw = b64.decode(data).map_err(|e| format!("a record's data is not base64: {e}"))?;
        let raw = if raw.starts_with(&[0x1f, 0x8b]) { gunzip(&raw, RECORD)? } else { raw };
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
