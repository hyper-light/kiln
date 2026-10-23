//! Datadog's Agent, taken (`docs/observe.md` §8.6): an Agent pointed at kiln (`dd_url`, and
//! `logs_config.logs_dd_url`), or writing to kiln beside Datadog (`additional_endpoints`), its API
//! key observe's ingest token. What an Agent 7.84 sends, at Datadog's own paths:
//!
//! - `POST /api/v2/series`: its metrics, protobuf (`kiln_obs::ddagent::series_into`);
//!   `/api/v1/series` the same as JSON (older Agents, or `use_v2_api.series: false`);
//! - `POST /api/beta/sketches`: its distributions' sketches;
//! - `POST /api/v1/check_run`: service checks, records as DogStatsD's are (`source:service_check`);
//! - `POST /intake/`: events (kiln's events, `source:kiln.event`), and what it says of its host
//!   (its host metadata, its checks), kept by the infrastructure list (`super::infra`);
//! - `POST /api/v1/metadata`: its inventories, its host's kept by the infrastructure list;
//! - `POST /api/v2/logs`: its logs, records, their `ddtags` attributes;
//! - `POST /api/v0.2/traces`: its tracers' spans (its trace agent's, `apm_config.apm_dd_url`),
//!   into the span store (`kiln_obs::ddtrace`);
//! - `POST /api/v0.2/stats`: its counts of every span before it sampled, as kiln's trace metrics;
//! - `GET /api/v1/validate`: whether its key is one;
//!
//! zstd (what it compresses with), deflate or gzip, or none. Anything else under Datadog's paths
//! (its products' intakes) is answered 202 and passed over, so an Agent neither retries nor
//! complains; Datadog's host API (`/api/v1/hosts`, `/api/v1/host/…`) is kiln's own, read with a
//! session: the ingest token is refused there. A node behind on its writes answers 503: the Agent keeps its payload and
//! sends it again.

use std::io::Read as _;

use http::{Method, StatusCode, header};
use kiln_obs::logs::record::Record;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Observe;
use crate::http::server as srv;

/// A payload unzipped, at the most: an Agent's are some MB at the most (`serializer_max_payload_size`,
/// its logs' batches 5 MB).
pub const MAX_BODY: usize = 64 << 20;

/// The API key a request carries: `DD-API-KEY`, or `api_key=` in its query (`/api/v1/validate`'s).
pub fn api_key(req: &srv::Request) -> Option<String> {
    if let Some(k) = req.headers().get("dd-api-key").and_then(|v| v.to_str().ok()) {
        return Some(k.trim().to_owned());
    }
    srv::query(req).get("api_key").cloned()
}

/// Whether `path` (its segments) is one of Datadog's, answered here.
pub fn is_datadog(path: &[&str]) -> bool {
    matches!(path, ["api", "v1" | "v2" | "v0.2" | "beta", ..] | ["intake", ..])
}

thread_local! {
    /// A zstd decoder each thread keeps: its tables and buffers grown once, a payload allocates
    /// nothing to unzip.
    static ZSTD: std::cell::RefCell<kiln_obs::zstd::Decoder> = std::cell::RefCell::new(kiln_obs::zstd::Decoder::new());
}

pub(super) fn unzip(encoding: &str, body: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    out.clear();
    match encoding.trim().to_ascii_lowercase().as_str() {
        "zstd" => ZSTD
            .with(|d| match d.try_borrow_mut() {
                Ok(mut d) => d.decompress(body, out, MAX_BODY),
                Err(_) => kiln_obs::zstd::Decoder::new().decompress(body, out, MAX_BODY),
            })
            .map_err(|e| e.to_string()),
        "deflate" => read_bounded(flate2::read::ZlibDecoder::new(body), out),
        "gzip" => read_bounded(flate2::read::GzDecoder::new(body), out),
        "" | "identity" => {
            out.extend_from_slice(body);
            Ok(())
        }
        other => Err(format!("a content encoding of zstd, deflate or gzip, not {other}")),
    }
}

fn read_bounded(r: impl std::io::Read, out: &mut Vec<u8>) -> Result<(), String> {
    r.take(MAX_BODY as u64 + 1).read_to_end(out).map_err(|e| format!("the body does not unzip: {e}"))?;
    if out.len() > MAX_BODY { Err(format!("the body unzips past {MAX_BODY} bytes")) } else { Ok(()) }
}

fn accepted() -> srv::Response {
    srv::json(StatusCode::ACCEPTED, &json!({ "status": "ok" }))
}

fn behind() -> srv::Response {
    srv::with_header(srv::json(StatusCode::SERVICE_UNAVAILABLE, &json!({ "errors": ["behind on writes: nothing was taken; try again"] })), header::RETRY_AFTER, "1")
}

fn bad(why: &str) -> srv::Response {
    srv::json(StatusCode::BAD_REQUEST, &json!({ "errors": [why] }))
}

/// A Datadog path's request, its key already found good.
pub async fn answer(o: &Observe, path: &[&str], req: srv::Request) -> srv::Response {
    if matches!(path, ["api", "v1", "validate"]) {
        return srv::json(StatusCode::OK, &json!({ "valid": true }));
    }
    // DORA's API (`super::dora`): deployments and failures a CD system sends.
    if let ["api", "v2", "dora", rest @ ..] = path {
        return super::dora::http::datadog(rest, req).await;
    }
    // The Service Catalog's definitions (`super::catalog`).
    if let ["api", "v2", "services", "definitions", rest @ ..] = path {
        return super::catalog::http::datadog(rest, req).await;
    }
    // Datadog's notebooks API (`super::notebooks`): a writer's token reads and changes none.
    if matches!(path, ["api", "v1", "notebooks", ..]) {
        return super::notebooks::http::datadog();
    }
    // Datadog's incident API (`super::incidents`): a writer's token reads and changes none.
    if matches!(path, ["api", "v2", "incidents", ..]) {
        return super::incidents::http::datadog();
    }
    // Datadog's host API (`super::infra`): a writer's token reads and mutes no host.
    if matches!(path, ["api", "v1", "hosts", ..] | ["api", "v1", "host", ..]) {
        return super::infra::http::datadog();
    }
    if req.method() != Method::POST {
        return srv::json(StatusCode::METHOD_NOT_ALLOWED, &json!({ "errors": ["Datadog's intake takes POST"] }));
    }
    let what = match path {
        ["api", "v2", "series"] => What::Series,
        ["api", "v1", "series"] => What::SeriesV1,
        ["api", "beta", "sketches"] => What::Sketches,
        ["api", "v1", "check_run"] => What::Checks,
        ["intake", ..] => What::Intake,
        ["api", "v1", "metadata"] => What::Metadata,
        ["api", "v2", "logs"] => What::Logs,
        ["api", "v0.2", "traces"] => What::Traces,
        ["api", "v0.2", "stats"] => What::Stats,
        _ => What::Else,
    };
    if matches!(what, What::Else) {
        return accepted();
    }
    let encoding = srv::header_str(&req, &header::CONTENT_ENCODING).unwrap_or("").to_owned();
    let body = match srv::body_bytes(req.into_body(), MAX_BODY + 1).await {
        Ok(b) => b,
        Err(_) => return srv::json(StatusCode::PAYLOAD_TOO_LARGE, &json!({ "errors": ["a payload is taken up to 64 MiB"] })),
    };
    let o = o.clone();
    let done = crate::rt::blocking(move || {
        if o.busy() {
            return behind();
        }
        let mut plain = o.bytes();
        let r = match unzip(&encoding, &body, &mut plain) {
            Ok(()) => take(&o, what, &plain),
            Err(why) => bad(&why),
        };
        o.give_bytes(plain);
        r
    })
    .await;
    done.unwrap_or_else(|e| srv::text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))
}

/// What a Datadog path takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Series,
    SeriesV1,
    Sketches,
    Checks,
    Intake,
    Metadata,
    Logs,
    Traces,
    Stats,
    Else,
}

/// A payload unzipped, taken by what it is.
fn take(o: &Observe, what: What, body: &[u8]) -> srv::Response {
    let now = super::clock_ms();
    match what {
        What::Series => {
            let mut d = o.decoding();
            match kiln_obs::ddagent::series_into(body, super::otlp::keys(), &mut d) {
                Ok(_) => {
                    if o.offer_samples(d) {
                        accepted()
                    } else {
                        behind()
                    }
                }
                Err(e) => {
                    o.give_decoding(d);
                    bad(&e.to_string())
                }
            }
        }
        What::SeriesV1 => match serde_json::from_slice::<SeriesV1>(body) {
            Ok(v) => offered(o.offer(v.series())),
            Err(e) => bad(&format!("{e}")),
        },
        What::Sketches => match kiln_obs::ddagent::sketches(body, super::otlp::keys()) {
            Ok((series, _)) => offered(o.offer(series)),
            Err(e) => bad(&e.to_string()),
        },
        What::Checks => match serde_json::from_slice::<Vec<CheckRun>>(body) {
            Ok(checks) => offered(o.offer_logs(checks.iter().map(|c| c.record(now)).collect())),
            Err(e) => bad(&format!("{e}")),
        },
        What::Intake => match serde_json::from_slice::<Value>(body) {
            Ok(v) => {
                // What it says of its host (its metadata, its checks), kept; its events taken.
                super::infra::heard_agent(&v);
                match Intake::deserialize(&v) {
                    Ok(i) => {
                        let records = i.records(now);
                        offered(records.is_empty() || o.offer_logs(records))
                    }
                    Err(_) => accepted(),
                }
            }
            // A payload of a shape this does not read: passed over, as the rest is.
            Err(_) => accepted(),
        },
        What::Metadata => {
            if let Ok(v) = serde_json::from_slice::<Value>(body) {
                super::infra::heard_agent(&v);
            }
            accepted()
        }
        What::Logs => match serde_json::from_slice::<Vec<LogIn>>(body) {
            Ok(logs) => offered(o.offer_logs(logs.into_iter().map(|l| l.record(now)).collect())),
            Err(e) => bad(&format!("{e}")),
        },
        What::Traces => {
            let mut spans = kiln_obs::otlp::traces::Spans::default();
            let decoded = with_trace_decoder(|d| {
                // Rows into buffers the node's writes handed back, as OTLP's traces are.
                while d.spares() < 2 {
                    let b = o.bytes();
                    if b.capacity() == 0 {
                        break;
                    }
                    d.give_rows(b);
                }
                d.decode(body, now.saturating_mul(1_000_000), &mut spans)
            });
            match decoded {
                Ok(()) => offered(o.offer_spans(spans.segments)),
                Err(e) => bad(&e.to_string()),
            }
        }
        What::Stats => {
            let mut d = o.decoding();
            match kiln_obs::ddtrace::stats_into(body, super::otlp::keys(), &mut d) {
                Ok(_) => offered(o.offer_samples(d)),
                Err(e) => {
                    o.give_decoding(d);
                    bad(&e.to_string())
                }
            }
        }
        What::Else => accepted(),
    }
}

/// Runs `f` with this thread's decoder of the Agent's traces (its room kept from one payload to
/// the next), or a new one were it in use.
pub(super) fn with_trace_decoder<T>(f: impl FnOnce(&mut kiln_obs::ddtrace::Decoder) -> T) -> T {
    thread_local! {
        static DECODER: std::cell::RefCell<kiln_obs::ddtrace::Decoder> = std::cell::RefCell::new(kiln_obs::ddtrace::Decoder::new());
    }
    DECODER.with(|d| match d.try_borrow_mut() {
        Ok(mut d) => f(&mut d),
        Err(_) => f(&mut kiln_obs::ddtrace::Decoder::new()),
    })
}

fn offered(taken: bool) -> srv::Response {
    if taken { accepted() } else { behind() }
}

/// `/api/v1/series`: Datadog's v1 series, as an Agent sends them.
#[derive(Debug, Deserialize)]
struct SeriesV1 {
    series: Vec<SerieV1>,
}

#[derive(Debug, Deserialize)]
struct SerieV1 {
    metric: String,
    points: Vec<(f64, f64)>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    interval: Option<i64>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

impl SeriesV1 {
    /// As `/api/v2/series`'s are: a rate the count it was, its value times its interval.
    fn series(self) -> Vec<(kiln_obs::batch::SeriesKey, Vec<(i64, f64)>)> {
        let mut out = Vec::with_capacity(self.series.len());
        for s in self.series {
            let (kind, scale) = match (s.kind.as_deref(), s.interval) {
                (Some("count"), _) => (kiln_obs::batch::Kind::Count, 1.0),
                (Some("rate"), Some(i)) if i > 0 => (kiln_obs::batch::Kind::Count, i as f64),
                _ => (kiln_obs::batch::Kind::Gauge, 1.0),
            };
            let mut tags: Vec<(String, String)> = Vec::new();
            for t in s.tags.iter().flatten() {
                let (k, v) = t.split_once(':').unwrap_or((t.as_str(), ""));
                if !k.is_empty() && !tags.iter().any(|(x, _)| x == k) {
                    tags.push((k.to_owned(), v.to_owned()));
                }
            }
            for (k, v) in [("host", &s.host), ("device", &s.device)] {
                if let Some(v) = v.as_deref().filter(|v| !v.is_empty())
                    && !tags.iter().any(|(x, _)| x == k)
                {
                    tags.push((k.to_owned(), v.to_owned()));
                }
            }
            let Ok(key) = kiln_obs::batch::SeriesKey::new(&s.metric, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), kind) else { continue };
            #[allow(clippy::cast_possible_truncation)]
            let points: Vec<(i64, f64)> = s.points.iter().filter(|(t, v)| t.is_finite() && v.is_finite()).map(|(t, v)| ((t * 1000.0) as i64, v * scale)).collect();
            if !points.is_empty() {
                out.push((key, points));
            }
        }
        out
    }
}

/// `/api/v1/check_run`: a service check's run.
#[derive(Debug, Deserialize)]
struct CheckRun {
    check: String,
    #[serde(default)]
    host_name: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    status: u8,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

impl CheckRun {
    fn record(&self, now_ms: i64) -> Record {
        let tags = self.tags.as_deref().unwrap_or_default().join(",");
        let c = kiln_obs::statsd::Check { name: &self.check, status: self.status.min(3), ts: self.timestamp, host: self.host_name.as_deref(), tags: &tags, message: self.message.as_deref().map(std::borrow::Cow::Borrowed) };
        kiln_obs::statsd::check_to_record(&c, "", &[], now_ms)
    }
}

/// `/intake/`: events by their source's name (and metadata, passed over).
#[derive(Debug, Deserialize)]
struct Intake {
    #[serde(default)]
    events: Option<std::collections::BTreeMap<String, Vec<EventIn>>>,
    #[serde(default, rename = "internalHostname")]
    host: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventIn {
    #[serde(default)]
    msg_title: String,
    #[serde(default)]
    msg_text: String,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    alert_type: Option<String>,
    #[serde(default)]
    aggregation_key: Option<String>,
    #[serde(default)]
    source_type_name: Option<String>,
}

impl Intake {
    fn records(&self, now_ms: i64) -> Vec<Record> {
        let mut out = Vec::new();
        for (source, events) in self.events.iter().flatten() {
            for e in events {
                if e.msg_title.trim().is_empty() {
                    continue;
                }
                let tags = e.tags.as_deref().unwrap_or_default().join(",");
                let ev = kiln_obs::statsd::Event {
                    title: std::borrow::Cow::Borrowed(&e.msg_title),
                    text: std::borrow::Cow::Borrowed(&e.msg_text),
                    ts: e.timestamp,
                    host: e.host.as_deref(),
                    key: e.aggregation_key.as_deref(),
                    priority: e.priority.as_deref(),
                    source: Some(e.source_type_name.as_deref().unwrap_or(source)),
                    alert: e.alert_type.as_deref(),
                    tags: &tags,
                };
                out.push(kiln_obs::statsd::event_to_record(&ev, self.host.as_deref().unwrap_or(""), &[], now_ms));
            }
        }
        out
    }
}

/// `/api/v2/logs`: a log record as the Agent sends it; its other fields attributes, as Datadog's
/// intake reads them.
#[derive(Debug, Deserialize)]
struct LogIn {
    message: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    ddsource: Option<String>,
    #[serde(default)]
    ddtags: Option<String>,
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}

impl LogIn {
    fn record(self, now_ms: i64) -> Record {
        let mut attrs: Vec<(String, String)> = Vec::new();
        for t in self.ddtags.as_deref().unwrap_or_default().split(',').filter(|t| !t.is_empty()) {
            let (k, v) = t.split_once(':').unwrap_or((t, ""));
            if !attrs.iter().any(|(x, _)| x == k) {
                attrs.push((k.to_owned(), v.to_owned()));
            }
        }
        if !self.rest.is_empty() {
            super::pipeline::grok::flatten(None, Value::Object(self.rest), 0, &mut attrs);
        }
        Record {
            ts: self.timestamp.unwrap_or(now_ms).saturating_mul(1_000_000),
            severity: self.status.as_deref().and_then(kiln_obs::logs::record::severities).map_or(9, |r| *r.start()),
            service: self.service.unwrap_or_default(),
            host: self.hostname.unwrap_or_default(),
            source: self.ddsource.unwrap_or_default(),
            message: self.message,
            attrs,
            trace_id: None,
            span_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JSON an Agent 7.84 sent, read as records.
    #[test]
    fn an_agent_s_json_reads_as_kiln_s_records() -> anyhow::Result<()> {
        let checks: Vec<CheckRun> = serde_json::from_str(r#"[{"check":"ntp.in_sync","host_name":"probe","timestamp":1792446650,"status":0,"message":"","tags":null},{"check":"db.up","host_name":"probe","timestamp":1792446655,"status":2,"message":"refused","tags":["service:db","env:prod"]}]"#)?;
        let r: Vec<Record> = checks.iter().map(|c| c.record(0)).collect();
        let [ok, down] = r.as_slice() else { anyhow::bail!("{r:?}") };
        anyhow::ensure!(ok.source == "service_check" && ok.severity == 9 && ok.host == "probe" && ok.message == "ntp.in_sync is OK" && ok.ts == 1_792_446_650_000_000_000);
        anyhow::ensure!(down.severity == 17 && down.service == "db" && down.message == "refused" && down.attrs.contains(&("env".into(), "prod".into())));
        let intake: Intake = serde_json::from_str(r#"{"apiKey":"","events":{"api":[{"msg_title":"title","msg_text":"text","timestamp":1792447364,"priority":"normal","host":"probe","tags":["env:test"],"alert_type":"warning"}],"System":[{"msg_title":"Agent Startup","msg_text":"Version 7.84.4","timestamp":1792447346,"host":"probe","source_type_name":"System","event_type":"Agent Startup"}]},"internalHostname":"probe"}"#)?;
        let r = intake.records(0);
        let [startup, ev] = r.as_slice() else { anyhow::bail!("{r:?}") };
        anyhow::ensure!(ev.source == "kiln.event" && ev.severity == 13 && ev.message == "title\n\ntext" && ev.attrs.contains(&("event.source".into(), "api".into())) && ev.attrs.contains(&("env".into(), "test".into())), "{ev:?}");
        anyhow::ensure!(startup.attrs.contains(&("event.source".into(), "System".into())) && startup.host == "probe");
        let logs: Vec<LogIn> = serde_json::from_str(r#"[{"message":"line 1","status":"error","timestamp":1792447368116,"hostname":"probe","service":"probe-svc","ddsource":"probe-src","ddtags":"filename:app.log,dirname:/logs","http":{"status_code":500}}]"#)?;
        let r: Vec<Record> = logs.into_iter().map(|l| l.record(0)).collect();
        let [l] = r.as_slice() else { anyhow::bail!("{r:?}") };
        anyhow::ensure!((l.service.as_str(), l.source.as_str(), l.host.as_str(), l.severity, l.ts) == ("probe-svc", "probe-src", "probe", 17, 1_792_447_368_116_000_000), "{l:?}");
        for (k, v) in [("filename", "app.log"), ("dirname", "/logs"), ("http.status_code", "500")] {
            anyhow::ensure!(l.attrs.contains(&(k.into(), v.into())), "{k}={v} in {:?}", l.attrs);
        }
        // v1 series: a rate the count it was.
        let v1: SeriesV1 = serde_json::from_str(r#"{"series":[{"metric":"app.hits","points":[[1800000000,2.5]],"type":"rate","interval":10,"host":"web-1","tags":["env:prod"]},{"metric":"app.temp","points":[[1800000000,21]],"type":"gauge"}]}"#)?;
        let s = v1.series();
        anyhow::ensure!(s.len() == 2 && s.first().is_some_and(|(k, p)| k.kind == kiln_obs::batch::Kind::Count && p == &vec![(1_800_000_000_000, 25.0)]), "{s:?}");
        Ok(())
    }

    #[test]
    fn datadog_s_paths_are_known_and_kiln_s_are_not_taken() {
        assert!(
            is_datadog(&["api", "v2", "series"])
                && is_datadog(&["intake", ""])
                && is_datadog(&["api", "beta", "sketches"])
                && is_datadog(&["api", "v1", "anything"])
                && is_datadog(&["api", "v0.2", "traces"])
        );
        assert!(!is_datadog(&["api", "observe", "v1", "status"]) && !is_datadog(&["api", "sync", "blobs", "x"]));
    }
}
