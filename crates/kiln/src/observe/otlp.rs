//! OTLP over HTTP (`docs/observe.md` §8): `POST /v1/metrics` and `POST /v1/logs`, as the
//! OpenTelemetry protocol specifies them (opentelemetry-proto v1.5.0, "OTLP/HTTP"): a request's body
//! in protobuf (`application/x-protobuf`) or JSON (`application/json`), gzip-compressed or not,
//! answered in the request's own encoding.
//!
//! JSON is put into protobuf's wire format here and read by the same decoder as protobuf
//! (`kiln_obs::otlp`), so one mapping serves both. The JSON is OTLP's "JSON Protobuf Encoding":
//! field names in lowerCamelCase (the proto's own names are taken too), 64-bit integers as numbers
//! or decimal strings, enums as integers, trace and span ids as hex, other bytes as base64; a field
//! of a name it does not know is skipped, as the specification requires. The walk keeps a stack of
//! its own, no recursion, and an `AnyValue` nested past [`MAX_DEPTH`] is dropped.
//!
//! A node too far behind on its writes answers 503 with `Retry-After`, which OTLP exporters retry
//! (the specification's retryable codes), rather than taking points it would drop.

use std::io::Read as _;

use kiln_obs::otlp::pb::Writer;
use serde_json::Value;

use super::Observe;

/// A request's body, at the most, compressed or not: the OpenTelemetry Collector's default for an
/// OTLP/HTTP receiver (`confighttp`, `max_request_body_size`, 20 MiB).
pub const MAX_BODY: usize = 20 << 20;
/// An `AnyValue` nested deeper is dropped: kiln reads a tag's or a body's value at most this deep
/// (`kiln_obs::otlp::metrics`'s rendering), and a hostile body cannot make the walk deep.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Metrics,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Msg(Schema),
    Str,
    /// Hex, as OTLP's JSON writes trace and span ids.
    Hex,
    /// Base64, as proto3's JSON writes other bytes.
    Base64,
    Fixed64,
    Fixed32,
    Double,
    /// uint32, uint64, int64 and enums: a varint.
    Varint,
    Sint,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schema {
    MetricsRequest,
    LogsRequest,
    ResourceMetrics,
    ResourceLogs,
    Resource,
    Scope,
    ScopeMetrics,
    ScopeLogs,
    Metric,
    Gauge,
    Sum,
    Histogram,
    ExpHistogram,
    Summary,
    NumberPoint,
    HistogramPoint,
    ExpPoint,
    Buckets,
    SummaryPoint,
    ValueAtQuantile,
    LogRecord,
    KeyValue,
    AnyValue,
    ArrayValue,
    KvList,
}

struct F {
    name: &'static str,
    proto: &'static str,
    num: u32,
    ty: Ty,
    repeated: bool,
}

const fn f(name: &'static str, proto: &'static str, num: u32, ty: Ty) -> F {
    F { name, proto, num, ty, repeated: false }
}

const fn r(name: &'static str, proto: &'static str, num: u32, ty: Ty) -> F {
    F { name, proto, num, ty, repeated: true }
}

/// Each message's fields, as opentelemetry-proto v1.5.0 numbers them (exemplars and other fields
/// kiln reads nothing from are left out, and so skipped).
fn fields(s: Schema) -> &'static [F] {
    use Schema as S;
    use Ty::*;
    match s {
        S::MetricsRequest => const { &[r("resourceMetrics", "resource_metrics", 1, Msg(S::ResourceMetrics))] },
        S::LogsRequest => const { &[r("resourceLogs", "resource_logs", 1, Msg(S::ResourceLogs))] },
        S::ResourceMetrics => const { &[f("resource", "resource", 1, Msg(S::Resource)), r("scopeMetrics", "scope_metrics", 2, Msg(S::ScopeMetrics)), f("schemaUrl", "schema_url", 3, Str)] },
        S::ResourceLogs => const { &[f("resource", "resource", 1, Msg(S::Resource)), r("scopeLogs", "scope_logs", 2, Msg(S::ScopeLogs)), f("schemaUrl", "schema_url", 3, Str)] },
        S::Resource => const { &[r("attributes", "attributes", 1, Msg(S::KeyValue)), f("droppedAttributesCount", "dropped_attributes_count", 2, Varint)] },
        S::Scope => const { &[f("name", "name", 1, Str), f("version", "version", 2, Str), r("attributes", "attributes", 3, Msg(S::KeyValue)), f("droppedAttributesCount", "dropped_attributes_count", 4, Varint)] },
        S::ScopeMetrics => const { &[f("scope", "scope", 1, Msg(S::Scope)), r("metrics", "metrics", 2, Msg(S::Metric)), f("schemaUrl", "schema_url", 3, Str)] },
        S::ScopeLogs => const { &[f("scope", "scope", 1, Msg(S::Scope)), r("logRecords", "log_records", 2, Msg(S::LogRecord)), f("schemaUrl", "schema_url", 3, Str)] },
        S::Metric => const { &[
            f("name", "name", 1, Str),
            f("description", "description", 2, Str),
            f("unit", "unit", 3, Str),
            f("gauge", "gauge", 5, Msg(S::Gauge)),
            f("sum", "sum", 7, Msg(S::Sum)),
            f("histogram", "histogram", 9, Msg(S::Histogram)),
            f("exponentialHistogram", "exponential_histogram", 10, Msg(S::ExpHistogram)),
            f("summary", "summary", 11, Msg(S::Summary)),
            r("metadata", "metadata", 12, Msg(S::KeyValue)),
        ] },
        S::Gauge => const { &[r("dataPoints", "data_points", 1, Msg(S::NumberPoint))] },
        S::Sum => const { &[r("dataPoints", "data_points", 1, Msg(S::NumberPoint)), f("aggregationTemporality", "aggregation_temporality", 2, Varint), f("isMonotonic", "is_monotonic", 3, Bool)] },
        S::Histogram => const { &[r("dataPoints", "data_points", 1, Msg(S::HistogramPoint)), f("aggregationTemporality", "aggregation_temporality", 2, Varint)] },
        S::ExpHistogram => const { &[r("dataPoints", "data_points", 1, Msg(S::ExpPoint)), f("aggregationTemporality", "aggregation_temporality", 2, Varint)] },
        S::Summary => const { &[r("dataPoints", "data_points", 1, Msg(S::SummaryPoint))] },
        S::NumberPoint => const { &[
            r("attributes", "attributes", 7, Msg(S::KeyValue)),
            f("startTimeUnixNano", "start_time_unix_nano", 2, Fixed64),
            f("timeUnixNano", "time_unix_nano", 3, Fixed64),
            f("asDouble", "as_double", 4, Double),
            // sfixed64: the same eight bytes as fixed64, two's complement.
            f("asInt", "as_int", 6, Fixed64),
            f("flags", "flags", 8, Varint),
        ] },
        S::HistogramPoint => const { &[
            r("attributes", "attributes", 9, Msg(S::KeyValue)),
            f("startTimeUnixNano", "start_time_unix_nano", 2, Fixed64),
            f("timeUnixNano", "time_unix_nano", 3, Fixed64),
            f("count", "count", 4, Fixed64),
            f("sum", "sum", 5, Double),
            r("bucketCounts", "bucket_counts", 6, Fixed64),
            r("explicitBounds", "explicit_bounds", 7, Double),
            f("flags", "flags", 10, Varint),
            f("min", "min", 11, Double),
            f("max", "max", 12, Double),
        ] },
        S::ExpPoint => const { &[
            r("attributes", "attributes", 1, Msg(S::KeyValue)),
            f("startTimeUnixNano", "start_time_unix_nano", 2, Fixed64),
            f("timeUnixNano", "time_unix_nano", 3, Fixed64),
            f("count", "count", 4, Fixed64),
            f("sum", "sum", 5, Double),
            f("scale", "scale", 6, Sint),
            f("zeroCount", "zero_count", 7, Fixed64),
            f("positive", "positive", 8, Msg(S::Buckets)),
            f("negative", "negative", 9, Msg(S::Buckets)),
            f("flags", "flags", 10, Varint),
            f("min", "min", 12, Double),
            f("max", "max", 13, Double),
            f("zeroThreshold", "zero_threshold", 14, Double),
        ] },
        S::Buckets => const { &[f("offset", "offset", 1, Sint), r("bucketCounts", "bucket_counts", 2, Varint)] },
        S::SummaryPoint => const { &[
            r("attributes", "attributes", 7, Msg(S::KeyValue)),
            f("startTimeUnixNano", "start_time_unix_nano", 2, Fixed64),
            f("timeUnixNano", "time_unix_nano", 3, Fixed64),
            f("count", "count", 4, Fixed64),
            f("sum", "sum", 5, Double),
            r("quantileValues", "quantile_values", 6, Msg(S::ValueAtQuantile)),
            f("flags", "flags", 8, Varint),
        ] },
        S::ValueAtQuantile => const { &[f("quantile", "quantile", 1, Double), f("value", "value", 2, Double)] },
        S::LogRecord => const { &[
            f("timeUnixNano", "time_unix_nano", 1, Fixed64),
            f("observedTimeUnixNano", "observed_time_unix_nano", 11, Fixed64),
            f("severityNumber", "severity_number", 2, Varint),
            f("severityText", "severity_text", 3, Str),
            f("body", "body", 5, Msg(S::AnyValue)),
            r("attributes", "attributes", 6, Msg(S::KeyValue)),
            f("droppedAttributesCount", "dropped_attributes_count", 7, Varint),
            f("flags", "flags", 8, Fixed32),
            f("traceId", "trace_id", 9, Hex),
            f("spanId", "span_id", 10, Hex),
            f("eventName", "event_name", 12, Str),
        ] },
        S::KeyValue => const { &[f("key", "key", 1, Str), f("value", "value", 2, Msg(S::AnyValue))] },
        S::AnyValue => const { &[
            f("stringValue", "string_value", 1, Str),
            f("boolValue", "bool_value", 2, Bool),
            f("intValue", "int_value", 3, Varint),
            f("doubleValue", "double_value", 4, Double),
            f("arrayValue", "array_value", 5, Msg(S::ArrayValue)),
            f("kvlistValue", "kvlist_value", 6, Msg(S::KvList)),
            f("bytesValue", "bytes_value", 7, Base64),
        ] },
        S::ArrayValue => const { &[r("values", "values", 1, Msg(S::AnyValue))] },
        S::KvList => const { &[r("values", "values", 1, Msg(S::KeyValue))] },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonError {
    #[error("not JSON: {0}")]
    Json(String),
    #[error("{0} is not an object")]
    NotObject(&'static str),
    #[error("field {0}: {1}")]
    Field(&'static str, &'static str),
}

fn u64_of(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_i64().map(|i| i as u64)),
        Value::String(s) => s.parse::<u64>().ok().or_else(|| s.parse::<i64>().ok().map(|i| i as u64)),
        _ => None,
    }
}

fn f64_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            s => s.parse().ok(),
        },
        _ => None,
    }
}

/// Writes one scalar field (or, repeated, a packed run of them).
fn scalar(w: &mut Writer, field: &F, v: &Value) -> Result<(), JsonError> {
    let bad = |why| JsonError::Field(field.name, why);
    if field.repeated {
        let Value::Array(items) = v else { return Err(bad("not an array")) };
        let mut packed = Writer::default();
        for it in items {
            match field.ty {
                Ty::Fixed64 => packed.buf.extend_from_slice(&u64_of(it).ok_or_else(|| bad("not a 64-bit integer"))?.to_le_bytes()),
                Ty::Double => packed.buf.extend_from_slice(&f64_of(it).ok_or_else(|| bad("not a number"))?.to_le_bytes()),
                Ty::Varint => kiln_obs::varint::put(&mut packed.buf, u64_of(it).ok_or_else(|| bad("not an integer"))?),
                _ => return Err(bad("a repeated field of this kind")),
            }
        }
        w.bytes(field.num, &packed.buf);
        return Ok(());
    }
    match field.ty {
        Ty::Str => w.str(field.num, v.as_str().ok_or_else(|| bad("not a string"))?),
        Ty::Hex => {
            let s = v.as_str().ok_or_else(|| bad("not a string"))?;
            if !s.is_empty() {
                w.bytes(field.num, &hex::decode(s).map_err(|_| bad("not hex"))?);
            }
        }
        Ty::Base64 => {
            use base64::Engine as _;
            let s = v.as_str().ok_or_else(|| bad("not a string"))?;
            w.bytes(field.num, &base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| bad("not base64"))?);
        }
        Ty::Fixed64 => w.fixed64(field.num, u64_of(v).ok_or_else(|| bad("not a 64-bit integer"))?),
        Ty::Fixed32 => {
            let n = u32::try_from(u64_of(v).ok_or_else(|| bad("not an integer"))?).map_err(|_| bad("past 32 bits"))?;
            kiln_obs::varint::put(&mut w.buf, (u64::from(field.num) << 3) | 5);
            w.buf.extend_from_slice(&n.to_le_bytes());
        }
        Ty::Double => w.double(field.num, f64_of(v).ok_or_else(|| bad("not a number"))?),
        Ty::Varint => w.varint(field.num, u64_of(v).ok_or_else(|| bad("not an integer"))?),
        Ty::Sint => {
            let n = match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            };
            w.varint(field.num, kiln_obs::varint::zig(n.ok_or_else(|| bad("not an integer"))?));
        }
        Ty::Bool => w.varint(field.num, u64::from(v.as_bool().ok_or_else(|| bad("not a boolean"))?)),
        Ty::Msg(_) => return Err(bad("a message where a value was")),
    }
    Ok(())
}

/// A message being written: its schema, its object's fields left, the field it is of in its parent,
/// and, while one of its repeated message fields is being written, that array's items left.
struct Frame<'v> {
    schema: Schema,
    entries: serde_json::map::Iter<'v>,
    num: u32,
    depth: usize,
    out: Writer,
    array: Option<(&'static F, std::slice::Iter<'v, Value>)>,
}

/// An OTLP/JSON request as the protobuf bytes it stands for.
pub fn json_to_pb(body: &[u8], signal: Signal) -> Result<Vec<u8>, JsonError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| JsonError::Json(e.to_string()))?;
    let Value::Object(root) = &v else { return Err(JsonError::NotObject("the request")) };
    let schema = match signal {
        Signal::Metrics => Schema::MetricsRequest,
        Signal::Logs => Schema::LogsRequest,
    };
    let mut stack: Vec<Frame<'_>> = vec![Frame { schema, entries: root.iter(), num: 0, depth: 0, out: Writer::default(), array: None }];
    loop {
        let Some(top) = stack.last_mut() else { return Ok(Vec::new()) };
        // A repeated message field's next item, if one is under way.
        if let Some((field, items)) = &mut top.array {
            let field: &'static F = field;
            match items.next() {
                Some(Value::Object(o)) => {
                    let Ty::Msg(s) = field.ty else { return Err(JsonError::Field(field.name, "not a message")) };
                    let depth = top.depth + usize::from(s == Schema::AnyValue);
                    if depth <= MAX_DEPTH {
                        stack.push(Frame { schema: s, entries: o.iter(), num: field.num, depth, out: Writer::default(), array: None });
                    }
                }
                Some(Value::Null) => {}
                Some(_) => return Err(JsonError::NotObject(field.name)),
                None => top.array = None,
            }
            continue;
        }
        match top.entries.next() {
            Some((key, value)) => {
                let Some(field) = fields(top.schema).iter().find(|f| f.name == key || f.proto == key) else { continue };
                if value.is_null() {
                    continue;
                }
                match (field.ty, field.repeated, value) {
                    (Ty::Msg(_), true, Value::Array(items)) => top.array = Some((field, items.iter())),
                    (Ty::Msg(s), false, Value::Object(o)) => {
                        let depth = top.depth + usize::from(s == Schema::AnyValue);
                        if depth <= MAX_DEPTH {
                            let num = field.num;
                            stack.push(Frame { schema: s, entries: o.iter(), num, depth, out: Writer::default(), array: None });
                        }
                    }
                    (Ty::Msg(_), _, _) => return Err(JsonError::Field(field.name, "not an object or an array of them")),
                    _ => scalar(&mut top.out, field, value)?,
                }
            }
            None => {
                let Some(done) = stack.pop() else { return Ok(Vec::new()) };
                match stack.last_mut() {
                    Some(parent) => parent.out.bytes(done.num, &done.out.buf),
                    None => return Ok(done.out.buf),
                }
            }
        }
    }
}

/// What a request came to.
#[derive(Debug)]
pub enum Answer {
    /// Taken: what the response says (the export service's response, empty, in the request's
    /// encoding).
    Ok { json: bool, points: u64 },
    /// Malformed: 400, and why.
    Bad(String),
    /// Too big: 413.
    TooLarge,
    /// This node is too far behind: 503, and when to try again.
    Busy,
}

/// The node's series keys, made once each, for every receiver of the node (the server's and a
/// runner's, on whichever thread decodes).
fn keys() -> &'static kiln_obs::otlp::metrics::Keys {
    static KEYS: std::sync::OnceLock<kiln_obs::otlp::metrics::Keys> = std::sync::OnceLock::new();
    KEYS.get_or_init(kiln_obs::otlp::metrics::Keys::new)
}

/// Takes an OTLP request's body: decompressed if gzip (bounded: a small body cannot unfold past
/// [`MAX_BODY`]), decoded by its content type, written to `o`.
pub fn take(o: &Observe, signal: Signal, content_type: &str, gzip: bool, body: &[u8], now_ns: i64) -> Answer {
    if body.len() > MAX_BODY {
        return Answer::TooLarge;
    }
    if o.busy() {
        return Answer::Busy;
    }
    let mut plain = Vec::new();
    let body = if gzip {
        let mut d = flate2::read::GzDecoder::new(body).take(MAX_BODY as u64 + 1);
        if d.read_to_end(&mut plain).is_err() {
            return Answer::Bad("the body is not gzip".into());
        }
        if plain.len() > MAX_BODY {
            return Answer::TooLarge;
        }
        plain.as_slice()
    } else {
        body
    };
    let json = content_type.split(';').next().is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"));
    let pb_owned;
    let pb = if json {
        match json_to_pb(body, signal) {
            Ok(b) => {
                pb_owned = b;
                pb_owned.as_slice()
            }
            Err(e) => return Answer::Bad(e.to_string()),
        }
    } else {
        body
    };
    match signal {
        Signal::Metrics => match kiln_obs::otlp::metrics::decode_with(pb, now_ns / 1_000_000, keys()) {
            Ok(d) => {
                let points = d.points;
                if !o.offer_samples(d.keys, d.samples) {
                    return Answer::Busy;
                }
                Answer::Ok { json, points }
            }
            Err(e) => Answer::Bad(e.to_string()),
        },
        Signal::Logs => match kiln_obs::otlp::logs::decode(pb, now_ns) {
            Ok(d) => {
                let n = d.records.len() as u64;
                if !o.offer_logs(d.records) {
                    return Answer::Busy;
                }
                Answer::Ok { json, points: n }
            }
            Err(e) => Answer::Bad(e.to_string()),
        },
    }
}

/// An OTLP request answered: read whole (at most [`MAX_BODY`]), taken on the blocking pool, and
/// answered as the specification asks (OTLP/HTTP "Response"): 200 with the export service's
/// response, empty, in the request's encoding; 400 for what cannot be decoded; 413 past the bound;
/// 503 with `Retry-After` while this node is behind.
pub async fn answer(o: &Observe, signal: Signal, req: crate::http::server::Request) -> crate::http::server::Response {
    use crate::http::server as srv;
    use http::{StatusCode, header};
    let content_type = srv::header_str(&req, &header::CONTENT_TYPE).unwrap_or("application/x-protobuf").to_owned();
    let gzip = srv::header_str(&req, &header::CONTENT_ENCODING).is_some_and(|e| e.trim().eq_ignore_ascii_case("gzip"));
    let body = match srv::body_bytes(req.into_body(), MAX_BODY + 1).await {
        Ok(b) => b,
        Err(_) => return srv::text(StatusCode::PAYLOAD_TOO_LARGE, "an OTLP request is taken up to 20 MiB"),
    };
    let (o, now_ns) = (o.clone(), i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)).unwrap_or(i64::MAX));
    let taken = crate::rt::blocking(move || take(&o, signal, &content_type, gzip, &body, now_ns)).await;
    match taken {
        Ok(Answer::Ok { json: true, .. }) => srv::response(StatusCode::OK, "application/json", srv::full("{}")),
        Ok(Answer::Ok { json: false, .. }) => srv::response(StatusCode::OK, "application/x-protobuf", srv::empty()),
        Ok(Answer::Bad(why)) => srv::text(StatusCode::BAD_REQUEST, &why),
        Ok(Answer::TooLarge) => srv::text(StatusCode::PAYLOAD_TOO_LARGE, "an OTLP request is taken up to 20 MiB"),
        Ok(Answer::Busy) => srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, "behind on writes: try again"), header::RETRY_AFTER, "1"),
        Err(e) => srv::text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    }
}

/// A runner's OTLP receiver on its loopback (`127.0.0.1:4318`, OTLP/HTTP's port, unless
/// `KILN_OTLP_ADDR` says another, or `off`): what an app on the machine sends to, as to a collector.
/// Loopback alone, so it asks no token.
struct Local {
    o: Observe,
}

impl crate::http::server::Handler for Local {
    async fn handle(self: std::sync::Arc<Self>, req: crate::http::server::Request) -> crate::http::server::Response {
        use crate::http::server as srv;
        use http::{Method, StatusCode};
        match (req.method().clone(), req.uri().path()) {
            (Method::POST, "/v1/metrics") => answer(&self.o, Signal::Metrics, req).await,
            (Method::POST, "/v1/logs") => answer(&self.o, Signal::Logs, req).await,
            (Method::GET, "/healthz") => srv::text(StatusCode::OK, "ok"),
            _ => srv::text(StatusCode::NOT_FOUND, "OTLP/HTTP: POST /v1/metrics or /v1/logs"),
        }
    }
}

/// The address a runner's receiver listens on, if any.
pub fn local_addr() -> Option<std::net::SocketAddr> {
    match std::env::var("KILN_OTLP_ADDR") {
        Ok(v) if v.trim().eq_ignore_ascii_case("off") => None,
        Ok(v) => v.trim().parse().ok(),
        Err(_) => Some(std::net::SocketAddr::from(([127, 0, 0, 1], 4318))),
    }
}

/// Serves the runner's receiver until `shutdown`. A port another collector holds is said and left.
pub async fn serve_local(addr: std::net::SocketAddr, o: Observe, shutdown: crate::rt::Shutdown) {
    use futures::FutureExt as _;
    let listener = match std::net::TcpListener::bind(addr).map_err(anyhow::Error::from).and_then(|l| hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(l), 128).map_err(|e| anyhow::anyhow!("{e}"))) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%addr, error = %format!("{e:#}"), "the OTLP receiver did not start (another collector on the port?)");
            return;
        }
    };
    tracing::info!(%addr, "OTLP/HTTP receiver listening");
    let handler = std::sync::Arc::new(Local { o });
    loop {
        let accepted = futures::select_biased! {
            () = shutdown.cancelled().fuse() => return,
            r = listener.accept().fuse() => r,
        };
        match accepted {
            Ok(stream) => {
                let (h, remote) = (handler.clone(), stream.peer_addr().ok());
                crate::rt::spawn_spread(async move {
                    crate::http::server::serve_accepted(stream, h, remote, None).await;
                });
            }
            Err(_) => crate::rt::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OTLP/JSON examples of the specification's repository, as they are
    /// (opentelemetry-proto v1.5.0 `examples/metrics.json` and `examples/logs.json`).
    const METRICS_JSON: &str = include_str!("fixtures/otlp-metrics.json");
    const LOGS_JSON: &str = include_str!("fixtures/otlp-logs.json");

    #[test]
    fn the_specification_s_metrics_example_reads_as_its_protobuf_would() -> anyhow::Result<()> {
        let pb = json_to_pb(METRICS_JSON.as_bytes(), Signal::Metrics)?;
        let d = kiln_obs::otlp::metrics::decode(&pb, 0)?;
        let names: std::collections::BTreeSet<String> = d.series().iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().map(|p| p.metric.to_owned())).collect();
        for want in ["my.counter", "my.gauge", "my.histogram.count", "my.histogram.sum", "my.histogram.bucket", "my.exponential.histogram.count", "my.exponential.histogram.p50"] {
            anyhow::ensure!(names.contains(want), "{want} missing from {names:?}");
        }
        let series = d.series();
        let gauge = series.iter().find(|(k, _)| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| p.metric == "my.gauge")).ok_or_else(|| anyhow::anyhow!("the gauge"))?;
        anyhow::ensure!(gauge.1 == vec![(1_544_712_660_300, 10.0)], "{:?}", gauge.1);
        let tags = kiln_obs::batch::parse_key(&gauge.0.key)?.tags;
        anyhow::ensure!(tags.contains(&("service", "my.service")) && tags.contains(&("my.gauge.attr", "some value")), "{tags:?}");
        Ok(())
    }

    #[test]
    fn the_specification_s_logs_example_reads_whole() -> anyhow::Result<()> {
        let pb = json_to_pb(LOGS_JSON.as_bytes(), Signal::Logs)?;
        let d = kiln_obs::otlp::logs::decode(&pb, 0)?;
        let r = d.records.first().ok_or_else(|| anyhow::anyhow!("a record"))?;
        anyhow::ensure!((r.ts, r.severity, r.message.as_str()) == (1_544_712_660_300_000_000, 10, "Example log record"), "{r:?}");
        anyhow::ensure!((r.service.as_str(), r.source.as_str()) == ("my.service", "my.library"));
        anyhow::ensure!(r.trace_id.map(hex::encode).as_deref() == Some("5b8efff798038103d269b633813fc60c"));
        anyhow::ensure!(r.span_id.map(hex::encode).as_deref() == Some("eee19b7ec3c1b174"));
        for (k, v) in [("string.attribute", "some string"), ("boolean.attribute", "true"), ("int.attribute", "10"), ("double.attribute", "637.704"), ("array.attribute", r#"["many","values"]"#), ("map.attribute", r#"{"some.map.key":"some value"}"#)] {
            anyhow::ensure!(r.attrs.contains(&(k.into(), v.into())), "{k}={v} in {:?}", r.attrs);
        }
        Ok(())
    }

    #[test]
    fn logs_in_json_read_with_hex_ids_and_unknown_fields_skipped() -> anyhow::Result<()> {
        let body = r#"{"resourceLogs":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"api"}}]},
            "scopeLogs":[{"scope":{"name":"app"},"logRecords":[{"timeUnixNano":"1700000000000000001","severityNumber":17,
            "body":{"stringValue":"boom"},"traceId":"5b8efff798038103d269b633813fc60c","spanId":"eee19b7ec3c1b174",
            "somethingNew":{"x":1},"attributes":[{"key":"n","value":{"intValue":"42"}},{"key":"deep","value":{"arrayValue":{"values":[{"boolValue":true}]}}}]}]}]}]}"#;
        let pb = json_to_pb(body.as_bytes(), Signal::Logs)?;
        let d = kiln_obs::otlp::logs::decode(&pb, 0)?;
        let r = d.records.first().ok_or_else(|| anyhow::anyhow!("a record"))?;
        anyhow::ensure!(r.ts == 1_700_000_000_000_000_001 && r.severity == 17 && r.message == "boom" && r.service == "api" && r.source == "app");
        anyhow::ensure!(r.trace_id.map(hex::encode).as_deref() == Some("5b8efff798038103d269b633813fc60c"));
        anyhow::ensure!(r.span_id.map(hex::encode).as_deref() == Some("eee19b7ec3c1b174"));
        anyhow::ensure!(r.attrs.contains(&("n".into(), "42".into())) && r.attrs.contains(&("deep".into(), "[true]".into())), "{:?}", r.attrs);
        Ok(())
    }

    #[test]
    fn hostile_json_is_an_error_or_nothing_never_a_panic_or_a_deep_walk() -> anyhow::Result<()> {
        // An AnyValue nested a hundred thousand deep: serde_json refuses it; one nested twenty deep
        // is walked to MAX_DEPTH and no further.
        let mut deep = String::from(r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"body":"#);
        for _ in 0..20 {
            deep.push_str(r#"{"arrayValue":{"values":["#);
        }
        deep.push_str(r#"{"stringValue":"x"}"#);
        for _ in 0..20 {
            deep.push_str("]}}");
        }
        deep.push_str("}]}]}]}");
        let pb = json_to_pb(deep.as_bytes(), Signal::Logs)?;
        let d = kiln_obs::otlp::logs::decode(&pb, 0)?;
        anyhow::ensure!(d.records.len() == 1);
        for bad in [r#"[]"#, r#"{"resourceLogs":7}"#, r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"traceId":"zz"}]}]}]}"#, "not json", r#"{"resourceMetrics":[{"scopeMetrics":[{"metrics":[{"gauge":{"dataPoints":[{"timeUnixNano":true}]}}]}]}]}"#] {
            anyhow::ensure!(json_to_pb(bad.as_bytes(), Signal::Logs).is_err() || json_to_pb(bad.as_bytes(), Signal::Metrics).is_err(), "{bad} taken");
        }
        Ok(())
    }
}
