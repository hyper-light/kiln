//! APM's API (`docs/observe.md` §7, §8.7): spans searched, aggregated and faceted as logs are,
//! their searches read as Datadog's trace explorer reads one (`kiln_obs::logs::search::parse_spans`);
//! a trace by its id, as a waterfall draws it; services, their resources and the calls they make,
//! from the trace metrics (`kiln_obs::apm::red`); the service map.

use std::collections::HashMap;

use http::StatusCode;
use kiln_obs::logs::record::Record;
use kiln_obs::logs::search::{Search, parse_spans};
use kiln_obs::logs::store::{Facet, Measure};
use kiln_obs::query::eval::Source;
use serde::Deserialize;
use serde_json::{Value, json};

use super::api::{AttrsOut, Fail, HexOut, RecordOut, apart, fail, read_apart};
use crate::http::server::{self as srv, Response};

/// Spans a search answers with at the most: a log search's.
const SPANS_MAX: usize = 1_000;
/// Spans of a trace it answers with at the most (the earliest; it says when there were more): what
/// a browser parses without a stall, some 10 MB of JSON at a kilobyte a span, a tenth of a second at
/// JSON.parse's 100 MB/s.
const TRACE_SPANS: usize = 10_000;
/// Spans a service map is made from at the most: past it, one trace in `k` (by its id, whole), its
/// counts `k` times what it saw. A span the map keeps takes some 100 bytes: 20 MB a map.
const MAP_SPANS: u64 = 200_000;
/// Facets a request asks for at the most, as the log explorer's.
const FACETS_MAX: usize = 32;

fn ns(ms: i64) -> i64 {
    ms.saturating_mul(1_000_000)
}

fn parse(q: &str) -> Result<Search, Fail> {
    parse_spans(q).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

#[derive(Debug, Deserialize)]
pub struct SearchBody {
    #[serde(default)]
    query: String,
    /// Milliseconds since the epoch.
    from: i64,
    to: i64,
    #[serde(default)]
    limit: Option<usize>,
}

/// The newest spans a search matches.
pub async fn search(body: SearchBody) -> Result<Response, Fail> {
    read_apart(kiln_obs::store::Store::spans_at, move |at, _| {
        let started = std::time::Instant::now();
        let parsed = parse(&body.query)?;
        let found = at.search(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), body.limit.unwrap_or(100).clamp(1, SPANS_MAX)).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let spans: Vec<RecordOut<'_>> = found.records.iter().map(RecordOut).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "spans": spans, "blocks": { "total": found.blocks_total, "pruned": found.blocks_pruned }, "rows_scanned": found.rows_scanned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 })))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct AggregateBody {
    #[serde(default)]
    query: String,
    from: i64,
    to: i64,
    /// Facets: `service`, `resource_name`, `host`, `status`, `@attribute`.
    #[serde(default)]
    by: Vec<String>,
    /// `count` (default), `cardinality`, `sum`, `avg`, `min`, `max`.
    #[serde(default)]
    measure: Option<String>,
    /// The facet or attribute measured (`@duration`, in ns).
    #[serde(default)]
    of: Option<String>,
    /// Buckets of this many ms: each group's value a bucket.
    #[serde(default)]
    step: Option<i64>,
}

/// Trace analytics: `count() by {service}`, `avg(@duration) by {resource_name}`, over a search.
pub async fn aggregate(body: AggregateBody) -> Result<Response, Fail> {
    let of = body.of.clone().unwrap_or_default().trim_start_matches('@').to_owned();
    let need = |m: Measure| if of.is_empty() { Err(fail(StatusCode::BAD_REQUEST, "this measure needs `of`, the attribute it measures")) } else { Ok(m) };
    let measure = match body.measure.as_deref().unwrap_or("count") {
        "count" => Measure::Count,
        "cardinality" => need(Measure::Cardinality(Facet::parse_span(&of)))?,
        "sum" => need(Measure::Sum(of.clone()))?,
        "avg" => need(Measure::Avg(of.clone()))?,
        "min" => need(Measure::Min(of.clone()))?,
        "max" => need(Measure::Max(of.clone()))?,
        other => return Err(fail(StatusCode::BAD_REQUEST, format!("a measure: {other} (count, cardinality, sum, avg, min, max)"))),
    };
    let by: Vec<Facet> = body.by.iter().map(|f| Facet::parse_span(f)).collect();
    read_apart(kiln_obs::store::Store::spans_at, move |at, _| {
        let started = std::time::Instant::now();
        let parsed = parse(&body.query)?;
        let names: Vec<String> = by.iter().map(Facet::name).collect();
        let group = |vals: &[String]| names.iter().zip(vals).map(|(k, x)| (k.clone(), Value::from(x.clone()))).collect::<serde_json::Map<_, _>>();
        if let Some(step) = body.step {
            if step <= 0 {
                return Err(fail(StatusCode::BAD_REQUEST, "a step: more than 0 ms"));
            }
            let b = at.aggregate_buckets(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), ns(step), &by, &measure).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            let series: Vec<Value> = b.groups.iter().map(|(vals, v)| json!({ "group": group(vals), "values": v })).collect();
            return Ok(srv::json(
                StatusCode::OK,
                &json!({ "grid": { "from": b.buckets.start / 1_000_000, "step": step, "n": b.buckets.n }, "series": series, "rows_scanned": b.rows_scanned, "blocks_pruned": b.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 }),
            ));
        }
        let agg = at.aggregate(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), &by, &measure).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let groups: Vec<Value> = agg.groups.iter().map(|(vals, v)| json!({ "group": group(vals), "value": v })).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "groups": groups, "rows_scanned": agg.rows_scanned, "blocks_pruned": agg.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 })))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct FacetsBody {
    #[serde(default)]
    query: String,
    from: i64,
    to: i64,
    facets: Vec<String>,
    #[serde(default)]
    top: Option<usize>,
}

/// The trace explorer's facets: each one's most common values over a search, in one pass.
pub async fn facets(body: FacetsBody) -> Result<Response, Fail> {
    let facets: Vec<Facet> = body.facets.iter().map(|f| Facet::parse_span(f)).collect();
    if facets.len() > FACETS_MAX {
        return Err(fail(StatusCode::BAD_REQUEST, format!("at most {FACETS_MAX} facets")));
    }
    let top = body.top.unwrap_or(10).clamp(1, 1000);
    read_apart(kiln_obs::store::Store::spans_at, move |at, _| {
        let started = std::time::Instant::now();
        let parsed = parse(&body.query)?;
        let fc = at.facets(&parsed, ns(body.from), ns(body.to).saturating_add(999_999), &facets, top).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
        let out: Vec<Value> = facets
            .iter()
            .zip(&fc.facets)
            .map(|(f, (values, distinct))| json!({ "facet": f.name(), "distinct": distinct, "values": values.iter().map(|(v, n)| json!({ "value": v, "count": n })).collect::<Vec<_>>() }))
            .collect();
        Ok(srv::json(
            StatusCode::OK,
            &json!({ "facets": out, "matched": fc.matched, "rows_scanned": fc.rows_scanned, "blocks_pruned": fc.blocks_pruned, "took_ms": started.elapsed().as_secs_f64() * 1000.0 }),
        ))
    })
    .await
}

/// A trace's span as the trace's answer gives it: the span as a search gives it, and its place.
struct PlacedOut<'a> {
    span: &'a Record,
    depth: u32,
    parent: Option<usize>,
    offset_ns: i64,
}

impl serde::Serialize for PlacedOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let r = self.span;
        let mut m = s.serialize_map(Some(14))?;
        m.serialize_entry("ts_ns", &r.ts)?;
        m.serialize_entry("offset_ns", &self.offset_ns)?;
        m.serialize_entry("duration_ns", &kiln_obs::apm::duration_of(&r.attrs))?;
        m.serialize_entry("depth", &self.depth)?;
        m.serialize_entry("parent", &self.parent)?;
        m.serialize_entry("status", kiln_obs::logs::record::level(r.severity))?;
        m.serialize_entry("service", &r.service)?;
        m.serialize_entry("host", &r.host)?;
        m.serialize_entry("source", &r.source)?;
        m.serialize_entry("resource_name", &r.message)?;
        m.serialize_entry("attributes", &AttrsOut(&r.attrs))?;
        m.serialize_entry("trace_id", &r.trace_id.map(HexOut))?;
        m.serialize_entry("span_id", &r.span_id.map(HexOut))?;
        m.end()
    }
}

fn hex_id<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

pub(super) fn range(q: &HashMap<String, String>, default_ms: i64) -> Result<(i64, i64), Fail> {
    let num = |k: &str| q.get(k).map(|v| v.parse::<i64>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{k}: milliseconds since the epoch")))).transpose();
    let to = num("to")?.unwrap_or_else(super::clock_ms);
    let from = num("from")?.unwrap_or(to - default_ms);
    if to < from {
        return Err(fail(StatusCode::BAD_REQUEST, "from before to"));
    }
    Ok((from, to))
}

/// A trace by its id: its spans (the earliest [`TRACE_SPANS`]) as a waterfall draws them, its
/// services, its errors. Within `from` and `to` (ms) when given (a link from a log or a span knows
/// its time), else whatever the spans' retention holds: the blocks without it are ruled out by
/// their filters, unread.
pub async fn trace(id: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let id = hex_id::<16>(id).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "a trace id: 32 hex digits"))?;
    let (from, to) = match (q.get("from"), q.get("to")) {
        (None, None) => (i64::MIN / 1_000_000, super::clock_ms() + 3_600_000),
        _ => range(q, 3_600_000)?,
    };
    // The spans as they are, read with the store let go.
    read_apart(kiln_obs::store::Store::spans_at, move |at, _| {
        let started = std::time::Instant::now();
        let mut spans: Vec<Record> = Vec::new();
        // Past the bound, the earliest are kept (a waterfall is read from its start): the latest
        // kept on top of a heap, replaced by an earlier one.
        let mut latest: std::collections::BinaryHeap<(i64, usize)> = std::collections::BinaryHeap::new();
        let mut matched = 0usize;
        at.each(&Search::trace(id), ns(from), ns(to).saturating_add(999_999), |r| {
            matched += 1;
            if spans.len() < TRACE_SPANS {
                latest.push((r.ts, spans.len()));
                spans.push(r.to_owned());
            } else if let Some(&(ts, i)) = latest.peek()
                && r.ts < ts
                && let Some(slot) = spans.get_mut(i)
            {
                *slot = r.to_owned();
                latest.pop();
                latest.push((r.ts, i));
            }
        })
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        if spans.is_empty() {
            return Err(fail(StatusCode::NOT_FOUND, "no spans of this trace (within the range, and the spans' retention)"));
        }
        let t = kiln_obs::apm::assemble(spans);
        let placed: Vec<PlacedOut<'_>> = t.spans.iter().map(|p| PlacedOut { span: &p.span, depth: p.depth, parent: p.parent, offset_ns: p.span.ts.saturating_sub(t.start) }).collect();
        let services: Vec<Value> = t.services.iter().map(|(k, n)| json!({ "service": k, "spans": n })).collect();
        let root = t.spans.first().map(|p| json!({ "service": p.span.service, "resource_name": p.span.message }));
        Ok(srv::json(
            StatusCode::OK,
            &json!({
                "trace_id": HexOut(id),
                "start_ns": t.start,
                "end_ns": t.end,
                "duration_ns": t.end.saturating_sub(t.start),
                "root": root,
                "spans_count": matched,
                "truncated": matched > t.spans.len(),
                "errors": t.errors,
                "roots": t.roots,
                "services": services,
                "spans": placed,
                "took_ms": started.elapsed().as_secs_f64() * 1000.0,
            }),
        ))
    })
    .await
}

fn red_json(r: &kiln_obs::apm::Red, key: &str) -> Value {
    let avg = (r.hits > 0.0).then(|| r.duration_sum / r.hits);
    let rate = (r.hits > 0.0).then(|| r.errors / r.hits);
    json!({ key: r.group, "hits": r.hits, "errors": r.errors, "error_rate": rate, "avg_s": avg, "p50_s": r.p50, "p95_s": r.p95, "p99_s": r.p99, "max_s": r.max })
}

pub(super) fn env_filter(q: &HashMap<String, String>) -> Option<String> {
    q.get("env").map(|e| e.trim().to_owned()).filter(|e| !e.is_empty() && e != "*")
}

/// The services: each one's requests over the range (an hour unless asked), their errors and
/// latency, from the trace metrics; `env` narrows them to one environment.
pub async fn services(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (from, to) = range(q, 3_600_000)?;
    let env = env_filter(q);
    // The trace metrics read a round of series at a time (`kiln_obs::query::eval::Lease`).
    apart(move |store| {
        let filter: Vec<(&str, &str)> = env.as_deref().map(|e| ("env", e)).into_iter().collect();
        let rows = kiln_obs::apm::red(Source::Shared(store), false, &filter, "service", from, to).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let envs: Vec<String> = store.read().unwrap_or_else(std::sync::PoisonError::into_inner).tag_values("env").into_iter().map(str::to_owned).collect();
        let services: Vec<Value> = rows.iter().filter(|r| r.hits > 0.0).map(|r| red_json(r, "service")).collect();
        Ok(srv::json(StatusCode::OK, &json!({ "from": from, "to": to, "env": env, "envs": envs, "services": services })))
    })
    .await
}

/// A service: its requests over the range, by resource, and the calls it makes, by what it calls.
pub async fn service(name: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (from, to) = range(q, 3_600_000)?;
    let env = env_filter(q);
    let name = name.to_owned();
    // The trace metrics read a round of series at a time (`kiln_obs::query::eval::Lease`).
    apart(move |store| {
        let mut filter: Vec<(&str, &str)> = vec![("service", name.as_str())];
        if let Some(e) = env.as_deref() {
            filter.push(("env", e));
        }
        let red = |outbound: bool, by: &str| kiln_obs::apm::red(Source::Shared(store), outbound, &filter, by, from, to).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e));
        let summary = red(false, "service")?;
        let resources = red(false, "resource_name")?;
        let kinds = red(false, "span.kind")?;
        let calls = red(true, "peer.service")?;
        if summary.iter().all(|r| r.hits == 0.0) && calls.iter().all(|r| r.hits == 0.0) {
            return Err(fail(StatusCode::NOT_FOUND, "no requests of this service, nor calls, in the range"));
        }
        Ok(srv::json(
            StatusCode::OK,
            &json!({
                "service": name,
                "from": from,
                "to": to,
                "env": env,
                "summary": summary.first().map(|r| red_json(r, "service")),
                "kinds": kinds.iter().map(|r| red_json(r, "span.kind")).collect::<Vec<_>>(),
                "resources": resources.iter().map(|r| red_json(r, "resource_name")).collect::<Vec<_>>(),
                "calls": calls.iter().map(|r| red_json(r, "peer.service")).collect::<Vec<_>>(),
            }),
        ))
    })
    .await
}

/// The service map over the range (15 minutes unless asked): who calls whom, from the spans, one
/// trace in `k` past [`MAP_SPANS`]; its figures from the trace metrics (`kiln_obs::apm::figures`),
/// the spans' (`k` times) where they have none.
pub async fn map(q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (from, to) = range(q, 15 * 60_000)?;
    let env = env_filter(q);
    let (a, b) = (ns(from), ns(to).saturating_add(999_999));
    let store = super::api::shared()?;
    // The spans as they are (and how many lie in the range) taken under a brief hold, read with the
    // store let go; the trace metrics' figures a round of series at a time.
    let take = move |s: &kiln_obs::store::Store| (s.spans_at(), s.span_rows_between(a, b));
    read_apart(take, move |(at, rows), _| {
        let started = std::time::Instant::now();
        let search = match env.as_deref() {
            Some(e) => parse(&format!("env:\"{}\"", e.replace('\\', "\\\\").replace('"', "\\\"")))?,
            None => Search::all(),
        };
        let k = rows.div_ceil(MAP_SPANS).max(1);
        let mut m = kiln_obs::apm::MapBuilder::new();
        at.each(&search, a, b, |r| {
            if kiln_obs::apm::sampled(r.trace_id, k) {
                m.take(r);
            }
        })
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let map = m.finish();
        // Its figures the trace metrics' (every request and call, a trace sampled away or not: an
        // Agent's stats and a runner's concentrator count those dropped), each edge its share of
        // the groups that count its calls; the spans' only where the metrics have none.
        let filter: Vec<(&str, &str)> = env.as_deref().map(|e| ("env", e)).into_iter().collect();
        let figures = kiln_obs::apm::figures(Source::Shared(&store), &map, &filter, from, to, k).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let nodes: Vec<Value> = figures.nodes.iter().map(|n| json!({ "service": n.service, "hits": n.hits, "errors": n.errors, "inferred": n.inferred })).collect();
        let edges: Vec<Value> =
            figures.edges.iter().map(|e| json!({ "from": e.from, "to": e.to, "calls": e.calls, "errors": e.errors, "avg_s": e.avg_s, "inferred": e.inferred, "counted": e.counted })).collect();
        Ok(srv::json(
            StatusCode::OK,
            &json!({ "from": from, "to": to, "nodes": nodes, "edges": edges, "spans_read": map.spans, "sampled_one_in": k, "took_ms": started.elapsed().as_secs_f64() * 1000.0 }),
        ))
    })
    .await
}
