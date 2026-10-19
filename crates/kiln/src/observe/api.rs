//! The observe API (`docs/observe.md` §7): JSON under `/api/observe/v1/`, what kiln's UI uses and
//! any client may. Queries are answered by this node's replica, as fresh as it applied, which every
//! answer states (`applied`); they run on the blocking pool under the store's read lock.

use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError};

use http::{Method, StatusCode, header};
use hyper::body::Incoming;
use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;
use kiln_obs::query::eval::{self, Request as QueryRequest};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Observe;
use crate::http::server::{self as srv, Request, Response};

pub type Fail = (StatusCode, String);

/// The node's group, once it started.
static OBSERVE: OnceLock<Observe> = OnceLock::new();

pub fn set_global(o: Observe) {
    let _ = OBSERVE.set(o);
}

pub fn get() -> Option<&'static Observe> {
    OBSERVE.get()
}

/// The server's monitors, once it started.
static MONITORS: OnceLock<std::sync::Arc<super::monitors::Monitors>> = OnceLock::new();

pub fn set_monitors(m: std::sync::Arc<super::monitors::Monitors>) {
    let _ = MONITORS.set(m);
}

fn monitors() -> Result<&'static std::sync::Arc<super::monitors::Monitors>, Fail> {
    MONITORS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "monitors are kept by the server"))
}

/// The server's dashboards, once it started.
static DASHBOARDS: OnceLock<std::sync::Arc<super::dashboards::Dashboards>> = OnceLock::new();

pub fn set_dashboards(d: std::sync::Arc<super::dashboards::Dashboards>) {
    let _ = DASHBOARDS.set(d);
}

fn dashboards() -> Result<&'static std::sync::Arc<super::dashboards::Dashboards>, Fail> {
    DASHBOARDS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "dashboards are kept by the server"))
}

/// The server's SLOs, once it started.
static SLOS: OnceLock<std::sync::Arc<super::slos::Slos>> = OnceLock::new();

pub fn set_slos(s: std::sync::Arc<super::slos::Slos>) {
    let _ = SLOS.set(s);
}

fn slos() -> Result<&'static std::sync::Arc<super::slos::Slos>, Fail> {
    SLOS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "SLOs are kept by the server"))
}

/// An SLO by its id, for a monitor of it.
pub fn slo_def(id: &str) -> Option<super::slos::SloDef> {
    SLOS.get()?.get(id)
}

/// The server's cloud integrations, once it started.
static CLOUDS: OnceLock<std::sync::Arc<super::cloud::Clouds>> = OnceLock::new();

/// The server's scrape targets, once it started.
static SCRAPES: OnceLock<std::sync::Arc<super::scrape::Targets>> = OnceLock::new();

pub fn set_scrape_targets(t: std::sync::Arc<super::scrape::Targets>) {
    let _ = SCRAPES.set(t);
}

fn scrapes() -> Result<&'static std::sync::Arc<super::scrape::Targets>, Fail> {
    SCRAPES.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "scrape targets are kept by the server"))
}

/// The server's synthetic tests, once it started.
static SYNTHETICS: OnceLock<std::sync::Arc<super::synthetics::Tests>> = OnceLock::new();

pub fn set_synthetics(t: std::sync::Arc<super::synthetics::Tests>) {
    let _ = SYNTHETICS.set(t);
}

fn synthetics() -> Result<&'static std::sync::Arc<super::synthetics::Tests>, Fail> {
    SYNTHETICS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "synthetic tests are kept by the server"))
}

/// The tests now, applied: the server's own run from now, each runner told its own.
fn apply_synthetics(t: &super::synthetics::Tests) {
    let all = t.list();
    if let Some(r) = super::synthetics::runner() {
        r.set(&all);
    }
    if let Some(o) = get() {
        o.set_synthetics(all);
    }
}

/// The server's log sources, once it started.
static LOG_SOURCES: OnceLock<std::sync::Arc<super::collect::Sources>> = OnceLock::new();

pub fn set_log_sources(s: std::sync::Arc<super::collect::Sources>) {
    let _ = LOG_SOURCES.set(s);
}

fn log_sources() -> Result<&'static std::sync::Arc<super::collect::Sources>, Fail> {
    LOG_SOURCES.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "log sources are kept by the server"))
}

/// The server's log pipelines, once it started.
static PIPELINES: OnceLock<std::sync::Arc<super::pipeline::kept::Pipelines>> = OnceLock::new();

pub fn set_pipelines(p: std::sync::Arc<super::pipeline::kept::Pipelines>) {
    let _ = PIPELINES.set(p);
}

fn pipelines() -> Result<&'static std::sync::Arc<super::pipeline::kept::Pipelines>, Fail> {
    PIPELINES.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "log pipelines are kept by the server"))
}

/// The server's metrics from logs, once it started.
static LOG_METRICS: OnceLock<std::sync::Arc<super::logmetrics::LogMetrics>> = OnceLock::new();

pub fn set_log_metrics(m: std::sync::Arc<super::logmetrics::LogMetrics>) {
    let _ = LOG_METRICS.set(m);
}

fn log_metrics() -> Result<&'static std::sync::Arc<super::logmetrics::LogMetrics>, Fail> {
    LOG_METRICS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "metrics from logs are counted by the server"))
}

/// A metric from logs as the API shows it: its definition, and what its latest window with records
/// of it held.
fn log_metric_json(ms: &super::logmetrics::LogMetrics, k: &super::logmetrics::Kept) -> Value {
    let mut v = serde_json::to_value(k).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("status".into(), ms.status(&k.def.name).map_or(Value::Null, |s| serde_json::to_value(s).unwrap_or(Value::Null)));
    }
    v
}

/// What a definition would count of the records held, window by window: a test, nothing written.
#[derive(Debug, Deserialize)]
struct LogMetricTest {
    metric: super::logmetrics::DefIn,
    /// ms; the last 15 minutes when not given, an hour at the most.
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
}

async fn log_metric_test(t: LogMetricTest) -> Result<Response, Fail> {
    use kiln_obs::logs::metrics::{WINDOW, window_of};
    // Its series named `preview` (`preview.count`, … a distribution's): what it is called is not
    // what it counts.
    let def = super::logmetrics::LogMetricDef { name: "preview".into(), ..t.metric.def(None) };
    let m = def.compile(0).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let to = t.to.unwrap_or_else(super::clock_ms);
    let from = t.from.unwrap_or(to - 15 * 60_000);
    if to < from || to - from > 3_600_000 {
        return Err(fail(StatusCode::BAD_REQUEST, "from before to, an hour apart at the most"));
    }
    let store = shared()?;
    let answer = crate::rt::blocking(move || -> Result<Value, String> {
        let metrics = [m];
        let mut series: std::collections::BTreeMap<Vec<u8>, Vec<(i64, f64)>> = std::collections::BTreeMap::new();
        let (mut records, mut rows) = (0u64, 0u64);
        let mut w = window_of(from.saturating_mul(1_000_000));
        while w <= to.saturating_mul(1_000_000) {
            // A hold of the store a window: writes go on between.
            let counted = store.read().unwrap_or_else(PoisonError::into_inner).count_log_metrics(&metrics, w).map_err(|e| e.to_string())?;
            if let Some(c) = counted {
                rows += c.rows_scanned;
                records += c.tallies.first().map_or(0, |t| t.records);
                for (k, p) in c.series {
                    series.entry(k.key.to_vec()).or_default().extend(p);
                }
            }
            w += WINDOW;
        }
        let out: Vec<Value> = series
            .iter()
            .filter_map(|(k, p)| {
                let key = kiln_obs::batch::parse_key(k).ok()?;
                let tags: serde_json::Map<String, Value> = key.tags.iter().map(|(k, v)| ((*k).to_owned(), Value::from(*v))).collect();
                Some(json!({ "metric": key.metric, "tags": tags, "points": p }))
            })
            .collect();
        Ok(json!({ "series": out, "records": records, "rows_scanned": rows }))
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(srv::json(StatusCode::OK, &answer))
}

fn log_metric_fail((code, e): (u16, String)) -> Fail {
    fail(StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST), e)
}

/// The pipelines now, applied: here from the next record, every runner told.
fn apply_pipelines(p: &super::pipeline::kept::Pipelines) {
    let defs = p.defs();
    super::pipeline::kept::activate(&defs);
    if let Some(o) = get() {
        o.set_pipelines(defs);
    }
}

/// A record to try pipelines on, as the API takes one.
#[derive(Debug, Deserialize)]
struct Sample {
    message: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    attributes: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct PipelineTest {
    /// The pipeline to try; none: every one applied now, in order.
    #[serde(default)]
    pipeline: Option<super::pipeline::PipelineDef>,
    records: Vec<Sample>,
}

/// The sources now, applied: the server's own read from now, each runner told its own.
fn apply_log_sources(s: &super::collect::Sources) {
    let all = s.list();
    if let Some(c) = super::collect::get() {
        c.set(&all);
    }
    if let Some(o) = get() {
        o.set_log_sources(all);
    }
}

/// The targets now, applied: the server's own read from now, each runner told its own.
fn apply_scrapes(t: &super::scrape::Targets) {
    let all = t.list();
    if let Some(s) = super::scrape::get() {
        s.set(&all);
    }
    if let Some(o) = get() {
        o.set_scrapes(all);
    }
}

pub fn set_clouds(c: std::sync::Arc<super::cloud::Clouds>) {
    let _ = CLOUDS.set(c);
}

fn clouds() -> Result<&'static std::sync::Arc<super::cloud::Clouds>, Fail> {
    CLOUDS.get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "cloud integrations are kept by the server"))
}

/// The node's view, each digest also in hex (`digest_hex`, `verified_hex`): a u64 is more than
/// JavaScript's numbers hold exactly, and two digests that differ must not read alike.
fn status_json(v: &super::View) -> Value {
    let hex = |d: u64| Value::from(format!("{d:016x}"));
    let mut j = serde_json::to_value(v).unwrap_or(Value::Null);
    if let Value::Object(o) = &mut j {
        o.insert("digest_hex".into(), hex(v.digest));
        o.insert("verified_hex".into(), v.verified.map_or(Value::Null, |(at, d)| json!([at, hex(d)])));
        if let Some(Value::Array(ms)) = o.get_mut("machines") {
            for (m, mv) in ms.iter_mut().zip(&v.machines) {
                if let (Some(s), Some(Value::Object(so))) = (&mv.status, m.get_mut("status")) {
                    so.insert("digest_hex".into(), hex(s.digest));
                    so.insert("verified_hex".into(), s.verified.map_or(Value::Null, |(at, d)| json!([at, hex(d)])));
                }
            }
        }
    }
    j
}

/// A monitor as the API shows it: its definition, its overall state and its groups'.
fn monitor_json(ms: &super::monitors::Monitors, m: &super::monitors::Monitor) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
    if let Value::Object(o) = &mut v {
        o.insert("state".into(), Value::from(ms.overall(&m.id)));
        o.insert("groups".into(), serde_json::to_value(ms.groups(&m.id)).unwrap_or(Value::Null));
        if let Ok(super::monitors::Compiled::Composite(c)) = m.compiled() {
            o.insert("composite".into(), composite_json(ms, &c));
        }
        // The downtimes that hold it now, or will.
        let now = crate::util::now_ms();
        let dts: Vec<Value> = ms.downtimes.of_monitor(&m.id, &m.tags, now).iter().map(|d| downtime_json(d, now)).collect();
        o.insert("downtimes".into(), Value::Array(dts));
    }
    v
}

/// A downtime as the API shows it: as kept, with its status and its window now (or its next).
fn downtime_json(d: &super::downtimes::Downtime, now: i64) -> Value {
    let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
    if let Value::Object(o) = &mut v {
        o.insert("status".into(), Value::from(d.status(now)));
        let w = d.window(now).map(|(s, e)| json!({ "start": s, "end": (e != i64::MAX).then_some(e) }));
        o.insert("window".into(), w.unwrap_or(Value::Null));
    }
    v
}

/// What a composite reads: each monitor with its letter (Datadog's UI's `a`, `b`, ...), name and
/// state, and the expression with their names.
fn composite_json(ms: &super::monitors::Monitors, c: &kiln_obs::monitor::composite::Composite) -> Value {
    let name = |id: &str| ms.get(id).map(|m| m.name);
    let monitors: Vec<Value> = c
        .ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let letter = char::from(b'a' + u8::try_from(i).unwrap_or(0));
            json!({ "letter": letter.to_string(), "id": id, "name": name(id), "state": ms.get(id).map(|_| ms.overall(id)) })
        })
        .collect();
    let lettered = c.show(|id| c.ids.iter().position(|x| x == id).map(|i| char::from(b'a' + u8::try_from(i).unwrap_or(0)).to_string()));
    json!({ "monitors": monitors, "expression": lettered, "named": c.show(|id| name(id)) })
}

/// The token OTLP exporters send the server (`Authorization: Bearer`), besides the admin's: one
/// that writes and reads nothing else (`<data>/observe/ingest.token`).
static INGEST: OnceLock<String> = OnceLock::new();

pub fn set_ingest_token(t: String) {
    let _ = INGEST.set(t);
}

/// Whether `token` may write over OTLP.
pub fn ingest_allowed(token: &str, admin: &str) -> bool {
    crate::util::ct_eq(token.as_bytes(), admin.as_bytes()) || INGEST.get().is_some_and(|t| crate::util::ct_eq(token.as_bytes(), t.as_bytes()))
}

/// `POST /v1/metrics` and `/v1/logs` on the server, for an authorised exporter.
pub async fn otlp(signal: super::otlp::Signal, req: Request, authorised: bool) -> Response {
    if !authorised {
        return srv::text(StatusCode::UNAUTHORIZED, "an OTLP exporter sends `Authorization: Bearer <the ingest token>`");
    }
    match get() {
        Some(o) => super::otlp::answer(o, signal, req).await,
        None => srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, "observe is starting"), header::RETRY_AFTER, "1"),
    }
}

/// OTLP/gRPC's `Export` on the server, for an authorised exporter (its token in gRPC's metadata,
/// `authorization: Bearer <the ingest token>`).
pub async fn otlp_grpc(signal: super::otlp::Signal, req: Request, authorised: bool) -> Response {
    use super::otlp::{grpc_answer, grpc_code};
    if !authorised {
        return grpc_answer(grpc_code::UNAUTHENTICATED, "an OTLP exporter sends authorization: Bearer <the ingest token>");
    }
    match get() {
        Some(o) => super::otlp::grpc(o, signal, req).await,
        None => grpc_answer(grpc_code::UNAVAILABLE, "observe is starting"),
    }
}

/// A request's body, at the most: a query's text is 8,000 bytes (`kiln_obs::query::lex`), an
/// ingest's points a few MB.
const QUERY_BODY: usize = 64 << 10;
const INGEST_BODY: usize = 16 << 20;
/// Names and values a listing gives, at the most.
const LIST: usize = 10_000;
/// Log records a search gives, at the most.
const LOGS_MAX: usize = 1_000;
/// Log records a search's patterns are found from, at the most (its newest).
const PATTERN_RECORDS: usize = 10_000;

fn fail(code: StatusCode, why: impl std::fmt::Display) -> Fail {
    (code, why.to_string())
}

fn observe() -> Result<&'static Observe, Fail> {
    get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "observe is starting"))
}

fn shared() -> Result<kiln_obs::machine::Shared, Fail> {
    observe()?.store().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "observe's store is not open yet"))
}

async fn read_json<T: serde::de::DeserializeOwned>(body: Incoming, limit: usize) -> Result<T, Fail> {
    srv::body_json(body, limit).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))
}

/// Runs `f` on the store under its read lock, on the blocking pool.
async fn reading<R: Send + 'static>(f: impl FnOnce(&kiln_obs::store::Store) -> Result<R, Fail> + Send + 'static) -> Result<R, Fail> {
    let store = shared()?;
    crate::rt::blocking(move || {
        let s = store.read().unwrap_or_else(PoisonError::into_inner);
        f(&s)
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
}

/// Each metric's samples a second of late: reckoned against what was counted at least ten seconds
/// before (the counts are kept, a minute at a time, here); none until there is such a count.
fn sample_rates(s: &kiln_obs::store::Store) -> std::collections::HashMap<String, f64> {
    type Counted = (std::time::Instant, std::collections::HashMap<String, u64>);
    static BEFORE: std::sync::Mutex<Option<Counted>> = std::sync::Mutex::new(None);
    let now: std::collections::HashMap<String, u64> = s.metric_samples().into_iter().map(|(n, c)| (n.to_owned(), c)).collect();
    let mut before = BEFORE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut rates = std::collections::HashMap::new();
    if let Some((at, then)) = before.as_ref() {
        let secs = at.elapsed().as_secs_f64();
        if secs >= 10.0 {
            for (name, &c) in &now {
                // A count lower than before: the store opened again; no rate.
                if let Some(&t) = then.get(name).filter(|&&t| t <= c) {
                    rates.insert(name.clone(), (c - t) as f64 / secs);
                }
            }
        }
    }
    if before.as_ref().is_none_or(|(at, _)| at.elapsed() >= std::time::Duration::from_secs(60)) {
        *before = Some((std::time::Instant::now(), now));
    }
    rates
}

/// A metric's summary as the API shows it.
fn summary_json(m: &kiln_obs::store::MetricSummary, rate: Option<f64>) -> Value {
    let kind = |k: kiln_obs::batch::Kind| match k {
        kiln_obs::batch::Kind::Gauge => "gauge",
        kiln_obs::batch::Kind::Count => "count",
        kiln_obs::batch::Kind::Counter => "counter",
    };
    json!({
        "name": m.name,
        "series": m.series,
        "kinds": m.kinds.iter().map(|(k, n)| json!({ "kind": kind(*k), "series": n })).collect::<Vec<_>>(),
        "tags": m.tags.iter().map(|t| {
            let mut v = json!({ "key": t.key, "values": t.values, "series": t.series });
            if !t.top.is_empty()
                && let Some(o) = v.as_object_mut()
            {
                o.insert("top".into(), json!(t.top.iter().map(|(value, n)| json!({ "value": value, "series": n })).collect::<Vec<_>>()));
            }
            v
        }).collect::<Vec<_>>(),
        "samples": m.samples,
        "samples_per_s": rate,
        "last_ms": m.last_ms,
    })
}

/// An SLO as the API shows it: its definition and its standing (`days`: day by day too).
fn slo_json(s: &kiln_obs::store::Store, d: &super::slos::SloDef, now: i64, days: bool) -> Value {
    let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("standing".into(), super::slos::standing(s, d, now, days));
    }
    v
}

/// Whether monitor `m` alerts on SLO `id`.
fn reads_slo(m: &super::monitors::Monitor, id: &str) -> bool {
    matches!(kiln_obs::monitor::parse(&m.query), Ok(q) if matches!(&q.kind, kiln_obs::monitor::Kind::Slo { id: of, .. } if of == id))
}

/// The authenticated routes, under `/api/observe/v1/`.
pub async fn route(rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let method = req.method().clone();
    match (&method, rest) {
        (&Method::POST, ["query"]) => query(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::GET, ["query"]) => {
            let body = QueryBody { query: q.get("query").cloned().unwrap_or_default(), from: num(q, "from")?, to: num(q, "to")?, step: q.get("step").map(|s| s.parse()).transpose().map_err(|_| fail(StatusCode::BAD_REQUEST, "step"))?, points: None, max_series: None, heatmap: None };
            query(body).await
        }
        (&Method::GET, ["metrics"]) => {
            let pattern = q.get("match").cloned();
            let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(LIST).min(LIST);
            reading(move |s| {
                let names: Vec<&str> = s.metric_names().into_iter().filter(|n| pattern.as_deref().is_none_or(|p| kiln_obs::index::glob(p, n))).take(limit).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "metrics": names })))
            })
            .await
        }
        (&Method::GET, ["metrics", "summary"]) => {
            // The metrics of most series first: what each is and costs.
            let pattern = q.get("match").cloned();
            let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 1000);
            reading(move |s| {
                let rates = sample_rates(s);
                let (matched, all) = s.metric_summaries(&|n| pattern.as_deref().is_none_or(|p| kiln_obs::index::glob(p, n)), limit, 0);
                let metrics: Vec<Value> = all.iter().map(|m| summary_json(m, rates.get(m.name.as_str()).copied())).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "matched": matched, "metrics": metrics })))
            })
            .await
        }
        (&Method::GET, ["metrics", "summary", name]) => {
            let name = (*name).to_owned();
            reading(move |s| {
                let rates = sample_rates(s);
                let m = s.metric_summary(&name, 10).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such metric"))?;
                Ok(srv::json(StatusCode::OK, &summary_json(&m, rates.get(m.name.as_str()).copied())))
            })
            .await
        }
        (&Method::GET, ["tags"]) => {
            let key = q.get("key").cloned().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "which tag: ?key="))?;
            let pattern = q.get("match").cloned();
            let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(LIST).min(LIST);
            reading(move |s| {
                let values: Vec<&str> = s.tag_values(&key).into_iter().filter(|v| pattern.as_deref().is_none_or(|p| kiln_obs::index::glob(p, v))).take(limit).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "key": key, "values": values })))
            })
            .await
        }
        (&Method::POST, ["logs", "search"]) => search(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::POST, ["logs", "aggregate"]) => aggregate(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::POST, ["logs", "facets"]) => facets(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::POST, ["logs", "patterns"]) => patterns(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::GET, ["logs", "tail"]) => tail(q, req),
        (&Method::POST, ["events"]) => post_event(read_json(req.into_body(), QUERY_BODY).await?),
        (&Method::POST, ["events", "search"]) => {
            let mut b: SearchBody = read_json(req.into_body(), QUERY_BODY).await?;
            b.query = if b.query.trim().is_empty() || b.query.trim() == "*" { "source:kiln.event".into() } else { format!("source:kiln.event ({})", b.query) };
            search(b).await
        }
        (&Method::GET, ["status"]) => Ok(srv::json(StatusCode::OK, &status_json(&observe()?.view()))),
        (&Method::POST, ["compact"]) => {
            observe()?.compact();
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "compacting": true })))
        }
        (&Method::POST, ["verify"]) => {
            observe()?.verify();
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "verifying": true })))
        }
        (&Method::GET, ["downtimes"]) => {
            let now = crate::util::now_ms();
            let list: Vec<Value> = monitors()?.downtimes.list().iter().map(|d| downtime_json(d, now)).collect();
            Ok(srv::json(StatusCode::OK, &json!({ "downtimes": list })))
        }
        (&Method::POST, ["downtimes"]) => {
            let mut d: super::downtimes::Downtime = read_json(req.into_body(), QUERY_BODY).await?;
            d.id.clear();
            let d = monitors()?.downtimes.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::CREATED, &downtime_json(&d, crate::util::now_ms())))
        }
        (&Method::GET, ["downtimes", id]) => {
            let d = monitors()?.downtimes.get(id).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such downtime"))?;
            Ok(srv::json(StatusCode::OK, &downtime_json(&d, crate::util::now_ms())))
        }
        (&Method::PUT, ["downtimes", id]) => {
            let ms = monitors()?;
            let id = (*id).to_owned();
            if ms.downtimes.get(&id).is_none() {
                return Err(fail(StatusCode::NOT_FOUND, "no such downtime"));
            }
            let mut d: super::downtimes::Downtime = read_json(req.into_body(), QUERY_BODY).await?;
            d.id = id;
            let d = ms.downtimes.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::OK, &downtime_json(&d, crate::util::now_ms())))
        }
        // Canceled, as Datadog's are: kept, its status `canceled`.
        (&Method::DELETE, ["downtimes", id]) => match monitors()?.downtimes.cancel(id).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such downtime")),
        },
        (&Method::GET, ["monitors"]) => {
            let ms = monitors()?;
            Ok(srv::json(StatusCode::OK, &json!({ "monitors": ms.list().iter().map(|m| monitor_json(ms, m)).collect::<Vec<_>>() })))
        }
        (&Method::POST, ["monitors"]) => {
            let ms = monitors()?;
            let mut m: super::monitors::Monitor = read_json(req.into_body(), QUERY_BODY).await?;
            m.id.clear();
            let m = ms.put(m).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::CREATED, &monitor_json(ms, &m)))
        }
        (&Method::POST, ["monitors", "validate"]) => {
            #[derive(Deserialize)]
            struct V {
                query: String,
                #[serde(rename = "type", default)]
                kind: Option<String>,
            }
            let v: V = read_json(req.into_body(), QUERY_BODY).await?;
            if v.kind.as_deref() == Some(super::monitors::COMPOSITE) {
                let ms = monitors()?;
                return Ok(match kiln_obs::monitor::composite::parse(&v.query) {
                    Ok(c) => match c.ids.iter().find(|id| ms.get(id).is_none_or(|m| m.is_composite())) {
                        Some(bad) => srv::json(StatusCode::OK, &json!({ "ok": false, "error": format!("{bad}: no monitor, or a composite") })),
                        None => srv::json(StatusCode::OK, &json!({ "ok": true, "kind": "composite", "composite": composite_json(ms, &c) })),
                    },
                    Err(e) => srv::json(StatusCode::OK, &json!({ "ok": false, "error": e.to_string() })),
                });
            }
            // What it watches besides, to draw: the metric query, or the log search, and its window.
            Ok(match kiln_obs::monitor::parse(&v.query) {
                Ok(q) => {
                    let (kind, watched, window_ms) = match &q.kind {
                        kiln_obs::monitor::Kind::Metric { query, window_ms, .. } => ("metric", json!({ "query": query }), window_ms),
                        kiln_obs::monitor::Kind::Change { query, window_ms, .. } => ("change", json!({ "query": query }), window_ms),
                        kiln_obs::monitor::Kind::Logs { search, window_ms, .. } => ("logs", json!({ "search": search }), window_ms),
                        // A forecast's window is ahead: `window_ms` its horizon.
                        kiln_obs::monitor::Kind::Forecast { query, ahead_ms, .. } => ("forecast", json!({ "query": query }), ahead_ms),
                        kiln_obs::monitor::Kind::Slo { id, burn } => ("slo", json!({ "slo": id }), &burn.map_or(3_600_000, |b| b.0)),
                    };
                    srv::json(StatusCode::OK, &json!({ "ok": true, "op": q.op.as_str(), "threshold": q.threshold, "kind": kind, "watched": watched, "window_ms": window_ms }))
                }
                Err(e) => srv::json(StatusCode::OK, &json!({ "ok": false, "error": e.to_string() })),
            })
        }
        (&Method::GET, ["monitors", id]) => {
            let ms = monitors()?;
            let m = ms.get(id).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such monitor"))?;
            Ok(srv::json(StatusCode::OK, &monitor_json(ms, &m)))
        }
        (&Method::PUT, ["monitors", id]) => {
            let ms = monitors()?;
            let id = (*id).to_owned();
            if ms.get(&id).is_none() {
                return Err(fail(StatusCode::NOT_FOUND, "no such monitor"));
            }
            let mut m: super::monitors::Monitor = read_json(req.into_body(), QUERY_BODY).await?;
            m.id = id;
            let m = ms.put(m).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::OK, &monitor_json(ms, &m)))
        }
        (&Method::DELETE, ["monitors", id]) => match monitors()?.delete(id) {
            Ok(true) => Ok(srv::status(StatusCode::NO_CONTENT)),
            Ok(false) => Err(fail(StatusCode::NOT_FOUND, "no such monitor")),
            Err(super::monitors::Refused::Read(by)) => Err(fail(StatusCode::CONFLICT, format!("composite monitors read it: {}; change or delete them first", by.join(", ")))),
            Err(super::monitors::Refused::Io(e)) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, e)),
        },
        (&Method::POST, ["monitors", id, "mute"]) => {
            #[derive(Deserialize, Default)]
            struct Mute {
                /// Unix ms; none: until unmuted.
                until: Option<i64>,
            }
            let id = (*id).to_owned();
            let mute: Mute = srv::body_json(req.into_body(), QUERY_BODY).await.unwrap_or_default();
            let ms = monitors()?;
            let m = ms.mute(&id, Some(mute.until.unwrap_or(i64::MAX))).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such monitor"))?;
            Ok(srv::json(StatusCode::OK, &monitor_json(ms, &m)))
        }
        (&Method::POST, ["monitors", id, "unmute"]) => {
            let ms = monitors()?;
            let m = ms.mute(id, None).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such monitor"))?;
            Ok(srv::json(StatusCode::OK, &monitor_json(ms, &m)))
        }
        (&Method::GET, ["integrations"]) => {
            let ms = monitors()?;
            let list: Vec<Value> = ms.integrations.list().iter().map(|(n, i)| {
                let mut v = i.redacted();
                if let Value::Object(o) = &mut v {
                    o.insert("name".into(), Value::from(n.clone()));
                }
                v
            }).collect();
            Ok(srv::json(StatusCode::OK, &json!({ "integrations": list })))
        }
        (&Method::PUT, ["integrations", name]) => {
            let name = (*name).to_owned();
            let i: super::notify::Integration = read_json(req.into_body(), QUERY_BODY).await?;
            monitors()?.integrations.put(&name, i).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["integrations", name]) => match monitors()?.integrations.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such integration")),
        },
        (&Method::POST, ["integrations", name, "test"]) => {
            monitors()?.test(name).map_err(|e| fail(StatusCode::NOT_FOUND, e))?;
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "sent": true })))
        }
        (&Method::GET, ["scrapes"]) => {
            // Each target, and how each machine that reads it goes: the server's own here, a
            // runner's from its last status.
            let mut by_target: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
            for (name, st) in super::scrape::get().map(|s| s.states()).unwrap_or_default() {
                by_target.entry(name).or_default().push(json!({ "machine": super::scrape::SERVER, "state": st }));
            }
            if let Some(o) = get() {
                for m in o.view().machines {
                    for (name, st) in m.status.map(|s| s.scrapes).unwrap_or_default() {
                        by_target.entry(name).or_default().push(json!({ "machine": m.machine, "state": st }));
                    }
                }
            }
            let list: Vec<Value> = scrapes()?
                .list()
                .into_iter()
                .map(|(name, t)| {
                    let mut v = t.redacted();
                    if let Value::Object(o) = &mut v {
                        o.insert("name".into(), Value::from(name.clone()));
                        o.insert("machines".into(), Value::from(by_target.remove(&name).unwrap_or_default()));
                    }
                    v
                })
                .collect();
            Ok(srv::json(StatusCode::OK, &json!({ "scrapes": list })))
        }
        (&Method::PUT, ["scrapes", name]) => {
            let name = (*name).to_owned();
            let t: super::scrape::Target = read_json(req.into_body(), QUERY_BODY).await?;
            let store = scrapes()?;
            store.put(&name, t).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            apply_scrapes(store);
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["scrapes", name]) => {
            let store = scrapes()?;
            match store.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
                true => {
                    apply_scrapes(store);
                    Ok(srv::status(StatusCode::NO_CONTENT))
                }
                false => Err(fail(StatusCode::NOT_FOUND, "no such scrape target")),
            }
        }
        (&Method::POST, ["scrapes", name, "test"]) => {
            // The target given (its token, if left out, the kept one's), or the kept one, read once
            // from the server: what it would get, nothing written.
            let name = (*name).to_owned();
            let given: Option<super::scrape::Target> = read_json::<Value>(req.into_body(), QUERY_BODY).await.ok().filter(|v| v.as_object().is_some_and(|o| !o.is_empty())).and_then(|v| serde_json::from_value(v).ok());
            let kept = scrapes()?.get(&name);
            let mut t = given.or_else(|| kept.clone()).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such scrape target"))?;
            if t.bearer.is_none() {
                t.bearer = kept.and_then(|k| k.bearer);
            }
            t.check().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let http = crate::http::client::Client::public().map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let t0 = std::time::Instant::now();
            let answer = match super::scrape::scrape_once(&http, &t).await {
                Ok((text, format)) => {
                    let s = kiln_obs::prom::parse_target(&text, super::clock_ms(), &[("job", name.as_str())], format);
                    let mut metrics: Vec<String> = s.series.iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().map(|k| k.metric.to_owned())).collect();
                    metrics.sort_unstable();
                    metrics.dedup();
                    json!({ "ok": true, "format": if format == kiln_obs::prom::Format::OpenMetrics { "openmetrics" } else { "text" }, "samples": s.samples, "skipped": s.skipped, "series": s.series.len(), "metrics": metrics.len(), "sample": metrics.iter().take(50).collect::<Vec<_>>(), "took_ms": t0.elapsed().as_millis() as u64 })
                }
                Err(e) => json!({ "ok": false, "error": format!("{e:#}"), "took_ms": t0.elapsed().as_millis() as u64 }),
            };
            Ok(srv::json(StatusCode::OK, &answer))
        }
        (&Method::GET, ["synthetics"]) => {
            // Each test, and how each location goes: the server's own here, a runner's from its
            // last status.
            let mut by_test: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
            for (name, st) in super::synthetics::runner().map(|r| r.states()).unwrap_or_default() {
                by_test.entry(name).or_default().push(json!({ "machine": super::synthetics::SERVER, "state": st }));
            }
            if let Some(o) = get() {
                for m in o.view().machines {
                    for (name, st) in m.status.map(|s| s.synthetics).unwrap_or_default() {
                        by_test.entry(name).or_default().push(json!({ "machine": m.machine, "state": st }));
                    }
                }
            }
            let list: Vec<Value> = synthetics()?
                .list()
                .into_iter()
                .map(|(name, t)| {
                    let mut v = t.redacted();
                    if let Value::Object(o) = &mut v {
                        o.insert("name".into(), Value::from(name.clone()));
                        o.insert("machines".into(), Value::from(by_test.remove(&name).unwrap_or_default()));
                        if t.alert.is_some() {
                            o.insert("monitor_id".into(), Value::from(super::synthetics::monitor_id(&name)));
                        }
                    }
                    v
                })
                .collect();
            Ok(srv::json(StatusCode::OK, &json!({ "synthetics": list })))
        }
        (&Method::PUT, ["synthetics", name]) => {
            let name = (*name).to_owned();
            let t: super::synthetics::Test = read_json(req.into_body(), QUERY_BODY).await?;
            t.check().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            // Its monitor checked before anything is kept: a test whose alerting does not compile
            // is refused whole.
            let owned = t.monitor(&name);
            if let Some(m) = &owned {
                m.compiled().map_err(|e| fail(StatusCode::BAD_REQUEST, format!("alert: {e}")))?;
            }
            let store = synthetics()?;
            store.put(&name, t).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            if let Ok(ms) = monitors() {
                match owned {
                    Some(m) => {
                        ms.put(m).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("alert: {e}")))?;
                    }
                    None => {
                        let _ = ms.delete(&super::synthetics::monitor_id(&name));
                    }
                }
            }
            apply_synthetics(store);
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["synthetics", name]) => {
            let store = synthetics()?;
            match store.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
                true => {
                    if let Ok(ms) = monitors() {
                        let _ = ms.delete(&super::synthetics::monitor_id(name));
                    }
                    apply_synthetics(store);
                    Ok(srv::status(StatusCode::NO_CONTENT))
                }
                false => Err(fail(StatusCode::NOT_FOUND, "no such synthetic test")),
            }
        }
        (&Method::POST, ["synthetics", name, "test"]) => {
            // The test given (its token, if left out or "…", the kept one's), or the kept one, run
            // once from the server: what it would see, nothing written.
            let name = (*name).to_owned();
            let given: Option<super::synthetics::Test> = read_json::<Value>(req.into_body(), QUERY_BODY).await.ok().filter(|v| v.as_object().is_some_and(|o| !o.is_empty())).and_then(|v| serde_json::from_value(v).ok());
            let kept = synthetics()?.get(&name);
            let mut t = given.or_else(|| kept.clone()).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such synthetic test"))?;
            if let (super::synthetics::Check::Http { bearer, .. }, Some(super::synthetics::Check::Http { bearer: old, .. })) = (&mut t.check, kept.map(|k| k.check))
                && (bearer.is_none() || bearer.as_deref() == Some("…"))
            {
                *bearer = old;
            }
            t.check().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let out = super::synthetics::run(&t).await;
            Ok(srv::json(StatusCode::OK, &serde_json::to_value(&out).unwrap_or(Value::Null)))
        }
        (&Method::POST, ["synthetics", name, "run"]) => {
            // Every location runs it now: the server here, each runner told.
            synthetics()?.get(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such synthetic test"))?;
            if let Some(r) = super::synthetics::runner() {
                r.run_now(name);
            }
            if let Some(o) = get() {
                o.run_synthetic(name);
            }
            Ok(srv::json(StatusCode::ACCEPTED, &json!({ "run": true })))
        }
        (&Method::GET, ["log_sources"]) => {
            // Each source, and how each machine that reads it goes: the server's own here, a
            // runner's from its last status.
            let mut by_source: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
            for (name, st) in super::collect::get().map(|c| c.states()).unwrap_or_default() {
                by_source.entry(name).or_default().push(json!({ "machine": super::collect::SERVER, "state": st }));
            }
            if let Some(o) = get() {
                for m in o.view().machines {
                    for (name, st) in m.status.map(|s| s.collect).unwrap_or_default() {
                        by_source.entry(name).or_default().push(json!({ "machine": m.machine, "state": st }));
                    }
                }
            }
            let list: Vec<Value> = log_sources()?
                .list()
                .into_iter()
                .map(|(name, s)| {
                    let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
                    if let Value::Object(o) = &mut v {
                        o.insert("name".into(), Value::from(name.clone()));
                        o.insert("machines".into(), Value::from(by_source.remove(&name).unwrap_or_default()));
                    }
                    v
                })
                .collect();
            Ok(srv::json(StatusCode::OK, &json!({ "log_sources": list })))
        }
        (&Method::PUT, ["log_sources", name]) => {
            let name = (*name).to_owned();
            let s: super::collect::Source = read_json(req.into_body(), QUERY_BODY).await?;
            let store = log_sources()?;
            store.put(&name, s).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            apply_log_sources(store);
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["log_sources", name]) => {
            let store = log_sources()?;
            match store.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
                true => {
                    apply_log_sources(store);
                    Ok(srv::status(StatusCode::NO_CONTENT))
                }
                false => Err(fail(StatusCode::NOT_FOUND, "no such log source")),
            }
        }
        (&Method::POST, ["log_sources", name, "test"]) => {
            // The source given, or the kept one, read on the server: what it matches here and the
            // records of its newest file's last lines, nothing written.
            let name = (*name).to_owned();
            let given: Option<super::collect::Source> = read_json::<Value>(req.into_body(), QUERY_BODY).await.ok().filter(|v| v.as_object().is_some_and(|o| !o.is_empty())).and_then(|v| serde_json::from_value(v).ok());
            let s = given.or_else(|| log_sources().ok().and_then(|l| l.get(&name))).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such log source"))?;
            let host = crate::os::hostname();
            let answer = crate::rt::blocking(move || super::collect::test(&name, &s, &host)).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(srv::json(StatusCode::OK, &answer))
        }
        (&Method::GET, ["pipelines"]) => Ok(srv::json(StatusCode::OK, &json!({ "pipelines": pipelines()?.list() }))),
        (&Method::POST, ["pipelines"]) => {
            let def: super::pipeline::PipelineDef = read_json(req.into_body(), QUERY_BODY).await?;
            let store = pipelines()?;
            let kept = store.create(def).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            apply_pipelines(store);
            Ok(srv::json(StatusCode::CREATED, &kept))
        }
        (&Method::GET, ["pipelines", id]) => pipelines()?.get(id).map(|p| srv::json(StatusCode::OK, &p)).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such pipeline")),
        (&Method::PUT, ["pipelines", id]) => {
            let id = (*id).to_owned();
            let def: super::pipeline::PipelineDef = read_json(req.into_body(), QUERY_BODY).await?;
            let store = pipelines()?;
            let kept = store.put(&id, def).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such pipeline"))?;
            apply_pipelines(store);
            Ok(srv::json(StatusCode::OK, &kept))
        }
        (&Method::DELETE, ["pipelines", id]) => {
            let store = pipelines()?;
            match store.delete(id).map_err(|e| fail(StatusCode::BAD_REQUEST, e))? {
                true => {
                    apply_pipelines(store);
                    Ok(srv::status(StatusCode::NO_CONTENT))
                }
                false => Err(fail(StatusCode::NOT_FOUND, "no such pipeline")),
            }
        }
        (&Method::PUT, ["pipeline_order"]) => {
            #[derive(Deserialize)]
            struct Order {
                pipeline_ids: Vec<String>,
            }
            let o: Order = read_json(req.into_body(), QUERY_BODY).await?;
            let store = pipelines()?;
            store.order(&o.pipeline_ids).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            apply_pipelines(store);
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::GET, ["log_metrics"]) => {
            let ms = log_metrics()?;
            let list: Vec<Value> = ms.list().iter().map(|k| log_metric_json(ms, k)).collect();
            Ok(srv::json(StatusCode::OK, &json!({ "log_metrics": list, "counting": ms.counting() })))
        }
        (&Method::POST, ["log_metrics"]) => {
            let def = read_json::<super::logmetrics::DefIn>(req.into_body(), QUERY_BODY).await?.def(None);
            let ms = log_metrics()?;
            let kept = reading(move |s| {
                // A metric's series have one writer: names written already are refused.
                let written = |n: &str| {
                    let mut ids = Vec::new();
                    s.select(&kiln_obs::index::Selector::metric(n), &mut ids);
                    !ids.is_empty()
                };
                ms.create(def, written).map_err(log_metric_fail)
            })
            .await?;
            Ok(srv::json(StatusCode::CREATED, &log_metric_json(log_metrics()?, &kept)))
        }
        (&Method::GET, ["log_metrics", name]) => {
            let ms = log_metrics()?;
            let k = ms.get(name).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such metric from logs"))?;
            Ok(srv::json(StatusCode::OK, &log_metric_json(ms, &k)))
        }
        (&Method::PUT, ["log_metrics", name]) => {
            let name = (*name).to_owned();
            let def = read_json::<super::logmetrics::DefIn>(req.into_body(), QUERY_BODY).await?.def(Some(&name));
            let ms = log_metrics()?;
            let k = ms.put(&name, def).map_err(log_metric_fail)?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such metric from logs"))?;
            Ok(srv::json(StatusCode::OK, &log_metric_json(ms, &k)))
        }
        (&Method::DELETE, ["log_metrics", name]) => match log_metrics()?.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such metric from logs")),
        },
        (&Method::POST, ["log_metric_test"]) => log_metric_test(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::POST, ["pipeline_test"]) => {
            // What a pipeline (or every one, in order) makes of records given: nothing kept.
            let t: PipelineTest = read_json(req.into_body(), QUERY_BODY).await?;
            if t.records.len() > 100 {
                return Err(fail(StatusCode::BAD_REQUEST, "100 records at the most"));
            }
            let now_ns = super::clock_ms().saturating_mul(1_000_000);
            let mut records: Vec<Record> = t
                .records
                .into_iter()
                .map(|s| Record {
                    ts: now_ns,
                    severity: s.status.as_deref().map_or(9, super::pipeline::status_of),
                    service: s.service,
                    host: s.host,
                    source: s.source,
                    message: s.message,
                    attrs: s.attributes.into_iter().map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect(),
                    ..Record::default()
                })
                .collect();
            super::pipeline::kept::try_on(t.pipeline.as_ref(), &mut records).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let out: Vec<Value> = records
                .iter()
                .map(|r| {
                    json!({
                        "ts_ns": r.ts,
                        "status": kiln_obs::logs::record::level(r.severity),
                        "service": r.service,
                        "host": r.host,
                        "source": r.source,
                        "message": r.message,
                        "attributes": r.attrs.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect::<serde_json::Map<_, _>>(),
                        "trace_id": r.trace_id.map(|t| t.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                    })
                })
                .collect();
            Ok(srv::json(StatusCode::OK, &json!({ "records": out })))
        }
        (&Method::GET, ["clouds"]) => Ok(srv::json(StatusCode::OK, &json!({ "clouds": clouds()?.list() }))),
        (&Method::PUT, ["clouds", name]) => {
            let name = (*name).to_owned();
            let c: super::cloud::Cloud = read_json(req.into_body(), QUERY_BODY).await?;
            clouds()?.put(&name, c).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::DELETE, ["clouds", name]) => match clouds()?.delete(name).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such cloud integration")),
        },
        (&Method::POST, ["clouds", name, "pull"]) => match clouds()?.pull_now(name) {
            true => Ok(srv::json(StatusCode::ACCEPTED, &json!({ "pulling": true }))),
            false => Err(fail(StatusCode::NOT_FOUND, "no such cloud integration")),
        },
        (&Method::POST, ["clouds", name, "test"]) => {
            // A configuration as the form holds it (its secrets `"…"`: the kept ones), pulled once,
            // nothing written.
            let name = (*name).to_owned();
            let c: super::cloud::Cloud = read_json(req.into_body(), QUERY_BODY).await?;
            Ok(srv::json(StatusCode::OK, &clouds()?.test(&name, c).await))
        }
        (&Method::GET, ["dashboards"]) => {
            // The list without the widgets: what a dashboard is, and how many it holds.
            let list: Vec<Value> = dashboards()?.list().iter().map(|d| json!({ "id": d.id, "title": d.title, "description": d.description, "builtin": d.builtin, "widgets": d.widgets.len(), "modified": d.modified })).collect();
            Ok(srv::json(StatusCode::OK, &json!({ "dashboards": list })))
        }
        (&Method::POST, ["dashboards"]) => {
            let mut d: super::dashboards::Dashboard = read_json(req.into_body(), super::dashboards::BYTES).await?;
            d.id.clear();
            let d = dashboards()?.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::CREATED, &d))
        }
        (&Method::GET, ["dashboards", id]) => Ok(srv::json(StatusCode::OK, &dashboards()?.get(id).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such dashboard"))?)),
        (&Method::PUT, ["dashboards", id]) => {
            let ds = dashboards()?;
            let id = (*id).to_owned();
            if ds.get(&id).is_none() {
                return Err(fail(StatusCode::NOT_FOUND, "no such dashboard"));
            }
            let mut d: super::dashboards::Dashboard = read_json(req.into_body(), super::dashboards::BYTES).await?;
            d.id = id;
            let d = ds.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::OK, &d))
        }
        (&Method::DELETE, ["dashboards", id]) => match dashboards()?.delete(id).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such dashboard")),
        },
        (&Method::POST, ["series"]) => ingest_series(read_json(req.into_body(), INGEST_BODY).await?),
        (&Method::POST, ["distribution_points"]) => ingest_distributions(read_json(req.into_body(), INGEST_BODY).await?),
        (&Method::POST, ["logs"]) => ingest_logs(read_json(req.into_body(), INGEST_BODY).await?),
        (&Method::GET, ["openapi.json"]) => Ok(srv::json(StatusCode::OK, &super::openapi::document(env!("CARGO_PKG_VERSION")))),
        (&Method::GET, ["slos"]) => {
            let list = slos()?.list();
            reading(move |s| {
                let now = crate::util::now_ms();
                let out: Vec<Value> = list.iter().map(|d| slo_json(s, d, now, false)).collect();
                Ok(srv::json(StatusCode::OK, &json!({ "slos": out })))
            })
            .await
        }
        (&Method::POST, ["slos"]) => {
            let mut d: super::slos::SloDef = read_json(req.into_body(), QUERY_BODY).await?;
            d.id.clear();
            let d = slos()?.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::CREATED, &d))
        }
        (&Method::GET, ["slos", id]) => {
            let d = slos()?.get(id).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such SLO"))?;
            let alerts: Vec<Value> = monitors().map(|ms| ms.list().iter().filter(|m| reads_slo(m, &d.id)).map(|m| json!({ "id": m.id, "name": m.name, "query": m.query, "state": ms.overall(&m.id) })).collect()).unwrap_or_default();
            reading(move |s| {
                let mut v = slo_json(s, &d, crate::util::now_ms(), true);
                if let Some(o) = v.as_object_mut() {
                    o.insert("alerts".into(), Value::from(alerts));
                }
                Ok(srv::json(StatusCode::OK, &v))
            })
            .await
        }
        (&Method::PUT, ["slos", id]) => {
            let store = slos()?;
            let id = (*id).to_owned();
            if store.get(&id).is_none() {
                return Err(fail(StatusCode::NOT_FOUND, "no such SLO"));
            }
            let mut d: super::slos::SloDef = read_json(req.into_body(), QUERY_BODY).await?;
            d.id = id;
            let d = store.put(d).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            Ok(srv::json(StatusCode::OK, &d))
        }
        (&Method::DELETE, ["slos", id]) => {
            let by: Vec<String> = monitors().map(|ms| ms.list().into_iter().filter(|m| reads_slo(m, id)).map(|m| m.name).collect()).unwrap_or_default();
            if !by.is_empty() {
                return Err(fail(StatusCode::CONFLICT, format!("monitors read it: {}; change or delete them first", by.join(", "))));
            }
            match slos()?.delete(id).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))? {
                true => Ok(srv::status(StatusCode::NO_CONTENT)),
                false => Err(fail(StatusCode::NOT_FOUND, "no such SLO")),
            }
        }
        _ => Err(fail(StatusCode::NOT_FOUND, "no such observe route")),
    }
}

fn num(q: &HashMap<String, String>, k: &str) -> Result<i64, Fail> {
    q.get(k).ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("?{k}= (ms since the epoch)")))?.parse().map_err(|_| fail(StatusCode::BAD_REQUEST, k))
}

#[derive(Debug, Deserialize)]
struct QueryBody {
    query: String,
    /// Milliseconds since the epoch.
    from: i64,
    to: i64,
    #[serde(default)]
    step: Option<i64>,
    #[serde(default)]
    points: Option<usize>,
    #[serde(default)]
    max_series: Option<usize>,
    /// A heatmap of the query instead of its series, of this many rows at the most
    /// (`kiln_obs::query::eval::heatmap`).
    #[serde(default)]
    heatmap: Option<usize>,
}

async fn query(body: QueryBody) -> Result<Response, Fail> {
    // What a query waits for, said with what it took: a thread to read on, then the store (held by
    // a write, or a cut).
    let asked = std::time::Instant::now();
    let store = shared()?;
    crate::rt::blocking(move || {
        let queued = asked.elapsed();
        let s = store.read().unwrap_or_else(PoisonError::into_inner);
        let waited = asked.elapsed() - queued;
        let s = &*s;
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let started = std::time::Instant::now();
        let mut r = QueryRequest::new(&body.query, body.from, body.to);
        // A forecast() forecasts past its series' last point, to the range's end.
        r.now = Some(crate::util::now_ms());
        r.step = body.step;
        if let Some(p) = body.points {
            r.points = p.clamp(1, eval::MAX_POINTS);
        }
        if let Some(m) = body.max_series {
            r.max_series = m.clamp(1, eval::DEFAULT_MAX_SERIES);
        }
        if let Some(rows) = body.heatmap {
            let h = eval::heatmap(s, &r, rows).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let rows: Vec<[f64; 2]> = h.rows.iter().map(|&(lo, hi)| [lo, hi]).collect();
            return Ok(srv::json(
                StatusCode::OK,
                &json!({
                    "grid": { "from": h.grid.from, "step": h.grid.step, "n": h.grid.n },
                    "heatmap": { "rows": rows, "counts": h.counts, "distribution": h.distribution },
                    "applied": s.applied(),
                    "took_ms": started.elapsed().as_secs_f64() * 1000.0,
                    "queued_ms": ms(queued),
                    "waited_ms": ms(waited),
                }),
            ));
        }
        let a = eval::eval(s, &r).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let series: Vec<Value> = a
            .series
            .iter()
            .map(|x| {
                let mut o = serde_json::Map::new();
                o.insert("tags".into(), Value::Object(x.tags.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect()));
                o.insert("values".into(), json!(x.values));
                // An `anomalies()` series' band: its bounds and each bucket's score (deviations).
                if let Some(b) = &x.bounds {
                    o.insert("bounds".into(), json!({ "lower": b.lower, "upper": b.upper, "score": b.score, "k": b.k, "direction": b.direction.as_str(), "algorithm": b.algorithm, "forecast_from": b.forecast_from }));
                }
                // An `outliers()` series': whether it is one.
                if let Some(out) = x.outlier {
                    o.insert("outlier".into(), Value::Bool(out));
                }
                Value::Object(o)
            })
            .collect();
        let mut grid = json!({ "from": a.grid.from, "step": a.grid.step, "n": a.grid.n });
        // A calendar rollup's buckets: each one's start (a month's days, a local day's hours, its own).
        if let (Some(edges), Some((c, _)), Some(o)) = (a.grid.edges(0), a.grid.calendar, grid.as_object_mut()) {
            o.insert("times".into(), json!(edges.get(..a.grid.n).unwrap_or(&[])));
            o.insert("calendar".into(), json!(c.unit.as_str()));
        }
        Ok(srv::json(
            StatusCode::OK,
            &json!({
                "grid": grid,
                "series": series,
                "scalar": a.scalar,
                "scanned": { "series": a.scanned_series, "samples": a.scanned_samples },
                "applied": s.applied(),
                "took_ms": started.elapsed().as_secs_f64() * 1000.0,
                "queued_ms": ms(queued),
                "waited_ms": ms(waited),
            }),
        ))
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
}

#[derive(Debug, Deserialize)]
struct SearchBody {
    #[serde(default)]
    query: String,
    /// Milliseconds since the epoch.
    from: i64,
    to: i64,
    #[serde(default)]
    limit: Option<usize>,
}

/// A search's patterns (`kiln_obs::logs::patterns`, Drain): its newest records (10,000 at the most)
/// grouped by template, oldest first as Drain reads a stream; each its count, when first and last
/// seen, its newest message, and its records by status; the most common first.
async fn patterns(body: SearchBody) -> Result<Response, Fail> {
    use kiln_obs::logs::patterns::{Drain, Params, tokens};
    reading(move |s| {
        let started = std::time::Instant::now();
        let parsed = kiln_obs::logs::search::parse(&body.query).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let ns = |ms: i64| ms.saturating_mul(1_000_000);
        let found = s.search_logs(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), body.limit.unwrap_or(PATTERN_RECORDS).clamp(1, PATTERN_RECORDS)).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        #[derive(Default)]
        struct Seen<'a> {
            count: u64,
            first: i64,
            last: i64,
            sample: &'a str,
            statuses: std::collections::BTreeMap<&'static str, u64>,
        }
        let params = Params::default();
        let mut drain = Drain::new(params);
        let mut seen: Vec<Seen<'_>> = Vec::new();
        for r in found.records.iter().rev() {
            let id = drain.add(&tokens(&r.message));
            if seen.len() <= id {
                seen.resize_with(id + 1, Seen::default);
            }
            if let Some(g) = seen.get_mut(id) {
                if g.count == 0 {
                    g.first = r.ts;
                }
                g.count += 1;
                g.last = r.ts;
                g.sample = &r.message;
                *g.statuses.entry(kiln_obs::logs::record::level(r.severity)).or_default() += 1;
            }
        }
        // Templates the tree kept apart, joined when the parameters say; then those that came out
        // alike are one pattern.
        let into: Vec<usize> = if params.merge { drain.merge() } else { (0..seen.len()).collect() };
        let mut joined: Vec<Seen<'_>> = Vec::with_capacity(seen.len());
        joined.resize_with(seen.len(), Seen::default);
        for (i, g) in seen.into_iter().enumerate() {
            let k = into.get(i).copied().unwrap_or(i);
            if let Some(into) = joined.get_mut(k) {
                if into.count == 0 {
                    into.first = g.first;
                }
                into.first = into.first.min(g.first);
                if g.last >= into.last {
                    into.last = g.last;
                    into.sample = g.sample;
                }
                into.count += g.count;
                for (s, n) in g.statuses {
                    *into.statuses.entry(s).or_default() += n;
                }
            }
        }
        let mut by_text: Vec<(String, Seen<'_>)> = Vec::new();
        for (c, g) in drain.clusters().iter().zip(joined).filter(|(_, g)| g.count > 0) {
            let text = c.text();
            match by_text.iter_mut().find(|(t, _)| *t == text) {
                Some((_, into)) => {
                    into.count += g.count;
                    into.first = into.first.min(g.first);
                    if g.last >= into.last {
                        into.last = g.last;
                        into.sample = g.sample;
                    }
                    for (k, v) in g.statuses {
                        *into.statuses.entry(k).or_default() += v;
                    }
                }
                None => by_text.push((text, g)),
            }
        }
        by_text.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
        let patterns: Vec<Value> = by_text.iter().map(|(t, g)| json!({ "template": t, "count": g.count, "first_ns": g.first, "last_ns": g.last, "sample": g.sample, "statuses": g.statuses })).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "patterns": patterns, "records": found.records.len(), "rows_scanned": found.rows_scanned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 })))
    })
    .await
}

async fn search(body: SearchBody) -> Result<Response, Fail> {
    reading(move |s| {
        let started = std::time::Instant::now();
        let parsed = kiln_obs::logs::search::parse(&body.query).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let ns = |ms: i64| ms.saturating_mul(1_000_000);
        let found = s.search_logs(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), body.limit.unwrap_or(100).clamp(1, LOGS_MAX)).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let out = SearchOut {
            records: RecordsOut(&found.records),
            blocks: BlocksOut { total: found.blocks_total, pruned: found.blocks_pruned },
            rows_scanned: found.rows_scanned,
            took_ms: started.elapsed().as_secs_f64() * 1000.0,
        };
        Ok(srv::json(StatusCode::OK, &out))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct AggregateBody {
    #[serde(default)]
    query: String,
    from: i64,
    to: i64,
    /// Facets: `service`, `host`, `source`, `status`, `@attribute`.
    #[serde(default)]
    by: Vec<String>,
    /// `count` (default), `cardinality`, `sum`, `avg`, `min`, `max`.
    #[serde(default)]
    measure: Option<String>,
    /// The facet or attribute measured (`@duration`).
    #[serde(default)]
    of: Option<String>,
    /// Buckets of this many ms (aligned to it): each group's value a bucket, a histogram.
    #[serde(default)]
    step: Option<i64>,
}

/// Log analytics: `count() by {service}` and the like over a search and a range.
async fn aggregate(body: AggregateBody) -> Result<Response, Fail> {
    use kiln_obs::logs::store::{Facet, Measure};
    let of = body.of.clone().unwrap_or_default().trim_start_matches('@').to_owned();
    let need = |m: Measure| if of.is_empty() { Err(fail(StatusCode::BAD_REQUEST, "this measure needs `of`, the attribute it measures")) } else { Ok(m) };
    let measure = match body.measure.as_deref().unwrap_or("count") {
        "count" => Measure::Count,
        "cardinality" => need(Measure::Cardinality(Facet::parse(&of)))?,
        "sum" => need(Measure::Sum(of.clone()))?,
        "avg" => need(Measure::Avg(of.clone()))?,
        "min" => need(Measure::Min(of.clone()))?,
        "max" => need(Measure::Max(of.clone()))?,
        other => return Err(fail(StatusCode::BAD_REQUEST, format!("a measure: {other} (count, cardinality, sum, avg, min, max)"))),
    };
    let by: Vec<Facet> = body.by.iter().map(|f| Facet::parse(f)).collect();
    reading(move |s| {
        let started = std::time::Instant::now();
        let parsed = kiln_obs::logs::search::parse(&body.query).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let ns = |ms: i64| ms.saturating_mul(1_000_000);
        let names: Vec<String> = by.iter().map(Facet::name).collect();
        let group = |vals: &[String]| names.iter().zip(vals).map(|(k, x)| (k.clone(), Value::from(x.clone()))).collect::<serde_json::Map<_, _>>();
        if let Some(step) = body.step {
            if step <= 0 {
                return Err(fail(StatusCode::BAD_REQUEST, "a step: more than 0 ms"));
            }
            let b = s.aggregate_logs_buckets(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), ns(step), &by, &measure).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let series: Vec<Value> = b.groups.iter().map(|(vals, v)| json!({ "group": group(vals), "values": v })).collect();
            return Ok(srv::json(
                StatusCode::OK,
                &json!({ "grid": { "from": b.buckets.start / 1_000_000, "step": step, "n": b.buckets.n }, "series": series, "rows_scanned": b.rows_scanned, "blocks_pruned": b.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 }),
            ));
        }
        let agg = s.aggregate_logs(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), &by, &measure).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let groups: Vec<Value> = agg.groups.iter().map(|(vals, v)| json!({ "group": group(vals), "value": v })).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "groups": groups, "rows_scanned": agg.rows_scanned, "blocks_pruned": agg.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 })))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct FacetsBody {
    #[serde(default)]
    query: String,
    from: i64,
    to: i64,
    /// `service`, `host`, `source`, `status`, `@attribute`.
    facets: Vec<String>,
    /// Values a facet shows at the most (10).
    #[serde(default)]
    top: Option<usize>,
}

/// The explorer's facet list: each facet's most common values over a search and a range, every
/// facet in one pass over the records.
async fn facets(body: FacetsBody) -> Result<Response, Fail> {
    use kiln_obs::logs::store::Facet;
    let facets: Vec<Facet> = body.facets.iter().map(|f| Facet::parse(f)).collect();
    if facets.len() > 32 {
        return Err(fail(StatusCode::BAD_REQUEST, "at most 32 facets"));
    }
    let top = body.top.unwrap_or(10).clamp(1, 1000);
    reading(move |s| {
        let started = std::time::Instant::now();
        let parsed = kiln_obs::logs::search::parse(&body.query).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let ns = |ms: i64| ms.saturating_mul(1_000_000);
        let fc = s.log_facets(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), &facets, top).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let out: Vec<Value> = facets.iter().zip(&fc.facets).map(|(f, (values, distinct))| json!({ "facet": f.name(), "distinct": distinct, "values": values.iter().map(|(v, n)| json!({ "value": v, "count": n })).collect::<Vec<_>>() })).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "facets": out, "matched": fc.matched, "rows_scanned": fc.rows_scanned, "blocks_pruned": fc.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 })))
    })
    .await
}

/// An event as Datadog's events API takes one (v1): a title and a text, tags `key:value`, an
/// alert type, its source and host, when it happened (seconds).
#[derive(Debug, Deserialize)]
struct EventIn {
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    tags: Vec<String>,
    /// `error`, `warning`, `info` (default), `success`.
    #[serde(default)]
    alert_type: Option<String>,
    #[serde(default)]
    source_type_name: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    date_happened: Option<i64>,
    #[serde(default)]
    aggregation_key: Option<String>,
}

fn post_event(e: EventIn) -> Result<Response, Fail> {
    let o = observe()?;
    if e.title.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "an event's title"));
    }
    let ts = e.date_happened.map_or_else(crate::util::now_ms, |s| s.saturating_mul(1000));
    let severity = match e.alert_type.as_deref() {
        Some("error") => 17,
        Some("warning") => 13,
        _ => 9,
    };
    let mut attrs: Vec<(String, String)> = vec![("event.type".into(), "custom".into()), ("title".into(), e.title.clone()), ("alert_type".into(), e.alert_type.clone().unwrap_or_else(|| "info".into()))];
    if let Some(k) = &e.aggregation_key {
        attrs.push(("aggregation_key".into(), k.clone()));
    }
    let mut service = String::new();
    for t in &e.tags {
        let (k, v) = t.split_once(':').unwrap_or((t.as_str(), ""));
        if k == "service" {
            service = v.to_owned();
        }
        attrs.push((k.to_owned(), v.to_owned()));
    }
    let message = if e.text.is_empty() { e.title.clone() } else { format!("{}\n\n{}", e.title, e.text) };
    if !o.offer_logs(vec![Record { ts: ts.saturating_mul(1_000_000), severity, service, host: e.host.unwrap_or_default(), source: "kiln.event".into(), message, attrs: { attrs.push(("event.source".into(), e.source_type_name.unwrap_or_else(|| "api".into()))); attrs }, trace_id: None, span_id: None }]) {
        return Ok(behind());
    }
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "status": "ok" })))
}

/// A log record as the API gives one (a search's, a tail's), written from the record as it is: the
/// JSON a `serde_json::Value` of it made, without a tree of values built a record. kiln's
/// serde_json keeps an object's keys in the order they were put in (`preserve_order`, which the
/// QUIC stack's qlog turns on): the fields in that order, the attributes in the order their keys
/// first come, a key given twice its last value.
struct RecordOut<'a>(&'a Record);

impl serde::Serialize for RecordOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let r = self.0;
        let mut m = s.serialize_map(Some(10))?;
        m.serialize_entry("ts_ns", &r.ts)?;
        m.serialize_entry("status", kiln_obs::logs::record::level(r.severity))?;
        m.serialize_entry("severity", &r.severity)?;
        m.serialize_entry("service", &r.service)?;
        m.serialize_entry("host", &r.host)?;
        m.serialize_entry("source", &r.source)?;
        m.serialize_entry("message", &r.message)?;
        m.serialize_entry("attributes", &AttrsOut(&r.attrs))?;
        m.serialize_entry("trace_id", &r.trace_id.map(HexOut))?;
        m.serialize_entry("span_id", &r.span_id.map(HexOut))?;
        m.end()
    }
}

/// Attributes as an object: each key where it first comes, with the last value it is given. A record
/// has a few: a key's earlier and later pairs are found by passes over them, nothing allocated;
/// past [`ATTRS_SCANNED`] they are sorted by key once instead.
struct AttrsOut<'a>(&'a [(String, String)]);

const ATTRS_SCANNED: usize = 32;

impl serde::Serialize for AttrsOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let a = self.0;
        let mut m = s.serialize_map(None)?;
        if a.len() > ATTRS_SCANNED {
            let key = |i: usize| a.get(i).map(|p| p.0.as_str());
            // Sorted by key, stably: a key's pairs in the order given, its first and last at the ends.
            let mut order: Vec<usize> = (0..a.len()).collect();
            order.sort_by(|&x, &y| key(x).cmp(&key(y)));
            let mut keys: Vec<(usize, usize)> = order.chunk_by(|&x, &y| key(x) == key(y)).filter_map(|run| Some((*run.first()?, *run.last()?))).collect();
            keys.sort_unstable();
            for (first, last) in keys {
                if let (Some((k, _)), Some((_, v))) = (a.get(first), a.get(last)) {
                    m.serialize_entry(k, v)?;
                }
            }
            return m.end();
        }
        for (i, (k, v)) in a.iter().enumerate() {
            if a.get(..i).unwrap_or_default().iter().any(|(e, _)| e == k) {
                continue;
            }
            let last = a.get(i..).unwrap_or_default().iter().rev().find(|(e, _)| e == k).map_or(v, |(_, v)| v);
            m.serialize_entry(k, last)?;
        }
        m.end()
    }
}

/// Bytes as lowercase hex, written from the stack.
struct HexOut<const N: usize>([u8; N]);

impl<const N: usize> serde::Serialize for HexOut<N> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; 64];
        let mut n = 0;
        for x in self.0.iter().take(32) {
            for d in [x >> 4, x & 15] {
                if let Some(b) = buf.get_mut(n) {
                    *b = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                }
                n += 1;
            }
        }
        s.serialize_str(std::str::from_utf8(buf.get(..n).unwrap_or_default()).unwrap_or_default())
    }
}

/// Records, as a list of [`RecordOut`].
struct RecordsOut<'a>(&'a [Record]);

impl serde::Serialize for RecordsOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(RecordOut))
    }
}

/// A tail's records, as a list of [`RecordOut`].
struct KeptOut<'a>(&'a [(u64, Record)]);

impl serde::Serialize for KeptOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.iter().map(|(_, r)| RecordOut(r)))
    }
}

/// A search's answer: its fields in the order a `Value` of it gave them.
#[derive(serde::Serialize)]
struct SearchOut<'a> {
    records: RecordsOut<'a>,
    blocks: BlocksOut,
    rows_scanned: u64,
    took_ms: f64,
}

#[derive(serde::Serialize)]
struct BlocksOut {
    total: u64,
    pruned: u64,
}

/// A tail's message.
#[derive(serde::Serialize)]
struct TailOut<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    records: KeptOut<'a>,
    read: u64,
    matched: u64,
    missed: u64,
    ms: u64,
}

/// A tail's message each [`TAIL_TICK`]: as live as a list is read (the eye takes in a moving list
/// a fixation at a time, 200–250 ms in reading: Rayner, "Eye movements in reading and information
/// processing: 20 years of research", Psychological Bulletin 124(3), 1998), and never redrawn
/// faster than that.
const TAIL_TICK: std::time::Duration = std::time::Duration::from_millis(200);
/// How often a tail reads the feed between its messages: what the ring holds for it stays a small
/// part of the ring's bound.
const TAIL_READ: std::time::Duration = std::time::Duration::from_millis(50);
/// Records a second a tail shows, sampled past it, unless asked otherwise: each row in view, of the
/// 25 or so a list shows, for two fixations (Rayner 1998), 25 / 0.5 s. Past it they come faster
/// than they can be read, and the tail says how many matched besides.
const TAIL_RATE: u32 = 50;
const TAIL_MAX_RATE: u32 = 1_000;
/// A tail with nothing to show says so this often, its counts at 0 (and its socket kept alive).
const TAIL_IDLE: std::time::Duration = std::time::Duration::from_secs(1);
/// A client that takes no message for this long is let go, and its place in the feed with it.
const TAIL_SEND_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// Live tail (`docs/observe.md` §6): over a WebSocket, the log records a search matches as this
/// node's replica applies them; past `rate` a second (50 unless asked) a uniformly random sample of
/// them, as Datadog's Live Tail. Each message holds a tick's records and how many rows the tick
/// read, matched and missed (a client too slow to keep up misses rows; it never holds the node up).
fn tail(q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let o = observe()?;
    let query = q.get("query").cloned().unwrap_or_default();
    let rate = q.get("rate").map(|r| r.parse::<u32>().ok().filter(|r| (1..=TAIL_MAX_RATE).contains(r))).unwrap_or(Some(TAIL_RATE));
    let (resp, upgrade) = crate::http::ws::accept_limited(req, 64 << 10).map_err(|r| fail(r.status, r.reason))?;
    // Taken before the upgrade completes: what is applied meanwhile is the tail's.
    let follower = o.feed().follow();
    crate::rt::spawn(async move {
        let socket = match upgrade.await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "a tail's upgrade failed");
                return;
            }
        };
        let started = match (kiln_obs::logs::search::parse(&query), rate, follower) {
            (Err(e), _, _) => Err(e.to_string()),
            (_, None, _) => Err(format!("a rate: 1 to {TAIL_MAX_RATE} records a second")),
            (_, _, None) => Err(format!("{} tails follow this node already; one must close first", kiln_obs::logs::tail::MAX_FOLLOWERS)),
            (Ok(search), Some(rate), Some(f)) => Ok((search, rate, f)),
        };
        let ended = match started {
            Ok((search, rate, f)) => follow_tail(socket, f, search, rate).await,
            Err(e) => refuse_tail(socket, &e).await,
        };
        if let Err(e) = ended {
            tracing::debug!(error = %format!("{e:#}"), "a tail ended");
        }
    });
    Ok(resp)
}

async fn refuse_tail(socket: crate::http::ws::WebSocket, why: &str) -> anyhow::Result<()> {
    let mut socket = socket;
    socket.send(crate::http::ws::text(json!({ "type": "error", "message": why }).to_string())).await?;
    socket.close(None).await.ok();
    Ok(())
}

async fn follow_tail(socket: crate::http::ws::WebSocket, follower: kiln_obs::logs::tail::Follower, search: kiln_obs::logs::search::Search, rate: u32) -> anyhow::Result<()> {
    use futures::{FutureExt as _, StreamExt as _};
    use std::time::Instant;
    use crate::http::ws::Message;
    // What a tick may show: its share of the rate, and what the ticks before left of theirs, up to a
    // second's worth. Records are applied in batches, so a tick often reads none; its share goes to
    // the next, and over a second what is shown is the rate asked whenever more than that match.
    let share = f64::from(rate) * TAIL_TICK.as_secs_f64();
    let mut allowance = share;
    let whole = |a: f64| a.floor() as usize;
    let (mut tx, mut rx) = socket.split();
    let mut state = Some((follower, kiln_obs::logs::tail::Sample::new(whole(allowance), rand::random()), search));
    let (mut next, mut said) = (Instant::now() + TAIL_TICK, Instant::now());
    // Each message is written here, then copied out once.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        futures::select_biased! {
            msg = rx.next().fuse() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => continue,
            },
            () = crate::rt::sleep(TAIL_READ).fuse() => {}
        }
        let Some((mut f, mut sample, search)) = state.take() else { return Ok(()) };
        let (f, mut sample, search) = if f.pending() {
            crate::rt::blocking(move || {
                let read = f.read(&search, &mut sample);
                if let Err(e) = read {
                    tracing::warn!(error = %e, "a tail read rows it could not decode");
                }
                (f, sample, search)
            })
            .await?
        } else {
            (f, sample, search)
        };
        let now = Instant::now();
        if now >= next {
            next = (next + TAIL_TICK).max(now);
            if sample.read > 0 || sample.missed > 0 || now.duration_since(said) >= TAIL_IDLE {
                let (read, matched, missed) = (sample.read, sample.matched, sample.missed);
                let kept = sample.kept();
                allowance -= kept.len() as f64;
                buf.clear();
                serde_json::to_writer(&mut buf, &TailOut { kind: "records", records: KeptOut(kept), read, matched, missed, ms: TAIL_TICK.as_millis() as u64 })?;
                let text = String::from_utf8_lossy(&buf).into_owned();
                match crate::rt::timeout(TAIL_SEND_WITHIN, tx.send(crate::http::ws::text(text))).await {
                    Ok(sent) => sent?,
                    Err(_) => return Ok(()),
                }
                said = now;
            }
            allowance = (allowance + share).min(f64::from(rate));
            sample.clear(whole(allowance));
        }
        state = Some((f, sample, search));
    }
}

/// Points as Datadog's series API takes them (v1): a metric, its type, its tags as `key:value`
/// (or an object), and points of `[seconds since the epoch, value]`.
#[derive(Debug, Deserialize)]
struct SeriesBody {
    series: Vec<SeriesIn>,
}

#[derive(Debug, Deserialize)]
struct SeriesIn {
    metric: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    tags: Option<Value>,
    #[serde(default)]
    host: Option<String>,
    points: Vec<(f64, f64)>,
}

fn tags_of(v: Option<&Value>, host: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|t| t.split_once(':').map_or((t.to_owned(), String::new()), |(k, v)| (k.to_owned(), v.to_owned()))).collect(),
        Some(Value::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect(),
        _ => Vec::new(),
    };
    if let Some(h) = host {
        out.push(("host".into(), h.to_owned()));
    }
    out
}

fn ingest_series(body: SeriesBody) -> Result<Response, Fail> {
    let o = observe()?;
    let mut series = Vec::with_capacity(body.series.len());
    let mut points = 0usize;
    for s in body.series {
        let kind = match s.kind.as_deref() {
            Some("count") => Kind::Count,
            Some("counter") => Kind::Counter,
            Some("gauge" | "rate") | None => Kind::Gauge,
            Some(other) => return Err(fail(StatusCode::BAD_REQUEST, format!("a series of type {other:?}: gauge, count, rate or counter"))),
        };
        let tags = tags_of(s.tags.as_ref(), s.host.as_deref());
        let key = SeriesKey::new(&s.metric, tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), kind).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{}: {e}", s.metric)))?;
        #[allow(clippy::cast_possible_truncation)]
        let p: Vec<(i64, f64)> = s.points.iter().filter(|(t, v)| t.is_finite() && v.is_finite()).map(|(t, v)| ((t * 1000.0) as i64, *v)).collect();
        points += p.len();
        series.push((key, p));
    }
    if !o.offer(series) {
        return Ok(behind());
    }
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "points": points })))
}

/// Distribution points as Datadog's API takes them (v1 `distribution_points`): a metric, its
/// tags, and points of `[seconds since the epoch, [values...]]`.
#[derive(Debug, Deserialize)]
struct DistributionsBody {
    series: Vec<DistributionIn>,
}

#[derive(Debug, Deserialize)]
struct DistributionIn {
    metric: String,
    #[serde(default)]
    tags: Option<Value>,
    #[serde(default)]
    host: Option<String>,
    points: Vec<(f64, Vec<f64>)>,
}

fn ingest_distributions(body: DistributionsBody) -> Result<Response, Fail> {
    let o = observe()?;
    let mut series = Vec::new();
    let mut values = 0usize;
    for d in body.series {
        let tags = tags_of(d.tags.as_ref(), d.host.as_deref());
        let borrowed: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        #[allow(clippy::cast_possible_truncation)]
        let points: Vec<(i64, Vec<f64>)> = d.points.into_iter().filter(|(t, _)| t.is_finite()).map(|(t, v)| ((t * 1000.0) as i64, v)).collect();
        values += points.iter().map(|(_, v)| v.len()).sum::<usize>();
        series.extend(kiln_obs::dist::series_of(&d.metric, &borrowed, &points).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{}: {e}", d.metric)))?);
    }
    if !o.offer(series) {
        return Ok(behind());
    }
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "values": values })))
}

/// Log records: a message, and what Datadog's intake takes beside it.
#[derive(Debug, Deserialize)]
struct LogIn {
    message: String,
    /// Milliseconds since the epoch; now when none.
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default, alias = "ddsource")]
    source: Option<String>,
    #[serde(default)]
    attributes: Option<serde_json::Map<String, Value>>,
}

fn ingest_logs(body: Vec<LogIn>) -> Result<Response, Fail> {
    let o = observe()?;
    let now = crate::util::now_ms();
    let n = body.len();
    let records: Vec<Record> = body
        .into_iter()
        .map(|l| Record {
            ts: l.timestamp.unwrap_or(now).saturating_mul(1_000_000),
            severity: l.status.as_deref().and_then(kiln_obs::logs::record::severities).map_or(9, |r| *r.start()),
            service: l.service.unwrap_or_default(),
            host: l.hostname.unwrap_or_default(),
            source: l.source.unwrap_or_default(),
            message: l.message,
            // Nested objects' keys joined by dots, as Datadog's intake reads them
            // (`{"http": {"status_code": 500}}` is `@http.status_code`).
            attrs: l.attributes.map_or_else(Vec::new, |a| {
                let mut out = Vec::with_capacity(a.len());
                super::pipeline::grok::flatten(None, Value::Object(a), 0, &mut out);
                out
            }),
            trace_id: None,
            span_id: None,
        })
        .collect();
    if !o.offer_logs(records) {
        return Ok(behind());
    }
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "records": n })))
}

/// The node is too far behind on its writes to take these: nothing of them was taken; send them
/// again in a second.
fn behind() -> Response {
    srv::with_header(srv::json(StatusCode::SERVICE_UNAVAILABLE, &json!({ "error": "behind on writes: nothing was taken; try again" })), header::RETRY_AFTER, "1")
}

/// `GET /api/observe/v1/snapshot/{snapshot}/{file}?machine=`: a snapshot's file for the machine
/// whose member fetches it, with that machine's token.
pub fn snapshot(snapshot: &str, file: &str, req: &Request) -> Result<Response, Fail> {
    let o = observe()?;
    let machine = srv::query(req).get("machine").cloned().unwrap_or_default();
    let given = srv::header_str(req, &header::AUTHORIZATION).and_then(|h| h.strip_prefix("Bearer ")).unwrap_or("");
    let want = o.token(&machine).ok_or_else(|| fail(StatusCode::NOT_FOUND, "this node serves no snapshots"))?;
    if machine.is_empty() || !crate::util::ct_eq(given.as_bytes(), want.as_bytes()) {
        return Err(fail(StatusCode::UNAUTHORIZED, "bad observe token"));
    }
    let path = o.snapshot_file(snapshot, file).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such snapshot file"))?;
    let body = crate::rt::file_source(path).map_err(|_| fail(StatusCode::NOT_FOUND, "no such snapshot file"))?;
    Ok(srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(body)))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(b: [u8; N]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The JSON a record was given as before: a `serde_json::Value` built of it.
    fn as_value(r: &Record) -> Value {
        json!({
            "ts_ns": r.ts,
            "status": kiln_obs::logs::record::level(r.severity),
            "severity": r.severity,
            "service": r.service,
            "host": r.host,
            "source": r.source,
            "message": r.message,
            "attributes": r.attrs.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect::<serde_json::Map<_, _>>(),
            "trace_id": r.trace_id.map(hex),
            "span_id": r.span_id.map(hex),
        })
    }

    /// Written from the record as it is, a record's JSON is byte for byte what a `Value` of it gave:
    /// fields in order, attributes where their keys first come (a key given twice its last value,
    /// past the count scanned as well as under it), text escaped, ids as hex.
    #[test]
    fn a_record_is_written_as_a_value_of_it_was() -> Result<(), serde_json::Error> {
        let pairs = |n: usize| -> Vec<(String, String)> { (0..n).map(|i| (format!("k{}", (i * 7919) % n), format!("v{i}"))).collect() };
        let mut many = pairs(ATTRS_SCANNED + 9);
        many.push(("k3".into(), "last".into()));
        let records = [
            Record::default(),
            Record {
                ts: -5,
                severity: 17,
                service: "api".into(),
                host: "h1".into(),
                source: "kiln".into(),
                message: "line \"one\"\n\ttab \u{1} and ünïcödé".into(),
                attrs: vec![("zeta".into(), "1".into()), ("alpha".into(), "2".into()), ("zeta".into(), "3".into()), ("".into(), "empty key".into())],
                trace_id: Some([0xab; 16]),
                span_id: Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
            },
            Record { ts: i64::MAX, severity: 9, message: "x".repeat(70_000), attrs: pairs(ATTRS_SCANNED), ..Record::default() },
            Record { ts: 1, severity: 24, attrs: many, ..Record::default() },
        ];
        for r in &records {
            assert_eq!(serde_json::to_string(&RecordOut(r))?, serde_json::to_string(&as_value(r))?);
        }
        let (blocks_pruned, blocks_total, rows_scanned) = (3u64, 9u64, 1234u64);
        let out = SearchOut { records: RecordsOut(&records), blocks: BlocksOut { total: blocks_total, pruned: blocks_pruned }, rows_scanned, took_ms: 1.5 };
        let before = json!({
            "records": records.iter().map(as_value).collect::<Vec<_>>(),
            "blocks": { "total": blocks_total, "pruned": blocks_pruned },
            "rows_scanned": rows_scanned,
            "took_ms": 1.5,
        });
        assert_eq!(serde_json::to_string(&out)?, serde_json::to_string(&before)?);
        Ok(())
    }
}
