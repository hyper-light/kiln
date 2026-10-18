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
    }
    v
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

/// A request's body, at the most: a query's text is 8,000 bytes (`kiln_obs::query::lex`), an
/// ingest's points a few MB.
const QUERY_BODY: usize = 64 << 10;
const INGEST_BODY: usize = 16 << 20;
/// Names and values a listing gives, at the most.
const LIST: usize = 10_000;
/// Log records a search gives, at the most.
const LOGS_MAX: usize = 1_000;

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

/// The authenticated routes, under `/api/observe/v1/`.
pub async fn route(rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let method = req.method().clone();
    match (&method, rest) {
        (&Method::POST, ["query"]) => query(read_json(req.into_body(), QUERY_BODY).await?).await,
        (&Method::GET, ["query"]) => {
            let body = QueryBody { query: q.get("query").cloned().unwrap_or_default(), from: num(q, "from")?, to: num(q, "to")?, step: q.get("step").map(|s| s.parse()).transpose().map_err(|_| fail(StatusCode::BAD_REQUEST, "step"))?, points: None, max_series: None };
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
            }
            let v: V = read_json(req.into_body(), QUERY_BODY).await?;
            // What it watches besides, to draw: the metric query, or the log search, and its window.
            Ok(match kiln_obs::monitor::parse(&v.query) {
                Ok(q) => {
                    let (kind, watched, window_ms) = match &q.kind {
                        kiln_obs::monitor::Kind::Metric { query, window_ms, .. } => ("metric", json!({ "query": query }), window_ms),
                        kiln_obs::monitor::Kind::Change { query, window_ms, .. } => ("change", json!({ "query": query }), window_ms),
                        kiln_obs::monitor::Kind::Logs { search, window_ms, .. } => ("logs", json!({ "search": search }), window_ms),
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
        (&Method::DELETE, ["monitors", id]) => match monitors()?.delete(id).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))? {
            true => Ok(srv::status(StatusCode::NO_CONTENT)),
            false => Err(fail(StatusCode::NOT_FOUND, "no such monitor")),
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
        (&Method::POST, ["logs"]) => ingest_logs(read_json(req.into_body(), INGEST_BODY).await?),
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
}

async fn query(body: QueryBody) -> Result<Response, Fail> {
    reading(move |s| {
        let started = std::time::Instant::now();
        let mut r = QueryRequest::new(&body.query, body.from, body.to);
        r.step = body.step;
        if let Some(p) = body.points {
            r.points = p.clamp(1, eval::MAX_POINTS);
        }
        if let Some(m) = body.max_series {
            r.max_series = m.clamp(1, eval::DEFAULT_MAX_SERIES);
        }
        let a = eval::eval(s, &r).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let series: Vec<Value> = a.series.iter().map(|x| json!({ "tags": x.tags.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect::<serde_json::Map<_, _>>(), "values": x.values })).collect();
        Ok(srv::json(
            StatusCode::OK,
            &json!({
                "grid": { "from": a.grid.from, "step": a.grid.step, "n": a.grid.n },
                "series": series,
                "scalar": a.scalar,
                "scanned": { "series": a.scanned_series, "samples": a.scanned_samples },
                "applied": s.applied(),
                "took_ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        ))
    })
    .await
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

async fn search(body: SearchBody) -> Result<Response, Fail> {
    reading(move |s| {
        let started = std::time::Instant::now();
        let parsed = kiln_obs::logs::search::parse(&body.query).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let ns = |ms: i64| ms.saturating_mul(1_000_000);
        let found = s.search_logs(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), body.limit.unwrap_or(100).clamp(1, LOGS_MAX)).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let records: Vec<Value> = found
            .records
            .iter()
            .map(|r| {
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
            })
            .collect();
        Ok(srv::json(
            StatusCode::OK,
            &json!({
                "records": records,
                "blocks": { "total": found.blocks_total, "pruned": found.blocks_pruned },
                "rows_scanned": found.rows_scanned,
                "took_ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        ))
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
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity, service, host: e.host.unwrap_or_default(), source: "kiln.event".into(), message, attrs: { attrs.push(("event.source".into(), e.source_type_name.unwrap_or_else(|| "api".into()))); attrs }, trace_id: None, span_id: None }]);
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "status": "ok" })))
}

fn hex<const N: usize>(b: [u8; N]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
    o.write(series);
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "points": points })))
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
            attrs: l.attributes.unwrap_or_default().into_iter().map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect(),
            trace_id: None,
            span_id: None,
        })
        .collect();
    o.log(records);
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "records": n })))
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

