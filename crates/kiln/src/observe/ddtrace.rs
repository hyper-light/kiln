//! Datadog's tracers, sent to kiln itself (`docs/observe.md` §8.7): each runner's loopback receiver
//! answers the trace agent's own API, and listens on its port too (`127.0.0.1:8126`, unless
//! `KILN_DD_TRACE_ADDR` says another, or `off`), so an app a Datadog tracer traces sends its traces
//! to the machine's kiln as to its Agent, no Agent beside it:
//!
//! - `PUT` or `POST /v0.4/traces` and `/v0.5/traces` (MessagePack): spans
//!   (`kiln_obs::ddtrace::Decoder::decode_tracer`), each counted first by this node's concentrator
//!   (`kiln_obs::ddtrace::Concentrator`: the trace metrics, of every span, as the Agent's stats
//!   count them), a trace its tracer dropped counted and not kept; answered with every service's
//!   rate 1 (`rate_by_service`: a tracer keeps every trace its own rules do not drop);
//! - `GET /info`: what it takes, those two alone, so a tracer computes no stats of its own and drops
//!   no trace for them;
//! - its other products' paths (telemetry, remote configuration, profiles, its event platform's
//!   proxy) answered and passed over, so a tracer neither retries nor complains.
//!
//! The concentrator's buckets are written by a task each second once due, as samples any node
//! takes; refused (the node behind), held and offered again the next second.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use http::{Method, StatusCode, header};
use kiln_obs::ddtrace::Concentrator;
use serde_json::json;

use super::Observe;
use crate::http::server as srv;

/// A tracer's request at the most: the trace agent's (`apm_config.max_payload_size`, 25 MiB).
pub const MAX_BODY: usize = 25 << 20;

/// This node's concentrator, and its name (the host its traces' Agent is).
static NODE: OnceLock<(String, Mutex<Concentrator>)> = OnceLock::new();

fn lock(m: &Mutex<Concentrator>) -> std::sync::MutexGuard<'_, Concentrator> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `path` is one of the trace agent's that tracers send to.
pub fn is_tracer(path: &str) -> bool {
    matches!(path, "/info" | "/v0.4/traces" | "/v0.5/traces" | "/v0.6/stats" | "/v0.7/config" | "/dogstatsd/v2/proxy")
        || ["/telemetry/proxy/", "/profiling/", "/evp_proxy/", "/debugger/", "/symdb/"].iter().any(|p| path.starts_with(p))
}

/// Starts the concentrator of this node (`host`) and the task that writes its buckets as they come
/// due, until `shutdown` (then every bucket open).
pub fn start(o: Observe, host: String, shutdown: crate::rt::Shutdown) {
    if NODE.set((host.clone(), Mutex::new(Concentrator::new(&host)))).is_err() {
        return;
    }
    crate::rt::spawn(async move {
        let mut held: Option<kiln_obs::otlp::metrics::Decoded> = None;
        loop {
            let going = !shutdown.sleep(Duration::from_secs(1)).await;
            let mut d = held.take().unwrap_or_else(|| o.decoding());
            if d.samples.is_empty()
                && let Some((_, c)) = NODE.get()
            {
                lock(c).flush_into(super::clock_ms(), going, super::otlp::keys(), &mut d);
            }
            if let Err(back) = o.offer_samples_or_back(d) {
                held = Some(*back);
            }
            if going {
                return;
            }
        }
    });
}

fn info() -> srv::Response {
    srv::json(
        StatusCode::OK,
        &json!({
            "version": format!("kiln {}", env!("CARGO_PKG_VERSION")),
            "endpoints": ["/v0.4/traces", "/v0.5/traces"],
            "client_drop_p0s": false,
            "span_meta_structs": false,
            "long_running_spans": false,
            "config": { "default_env": "none" }
        }),
    )
}

/// A tracer's request, on the loopback (no key asked).
pub async fn answer(o: &Observe, req: srv::Request) -> srv::Response {
    let path = req.uri().path().to_owned();
    let v5 = match (req.method(), path.as_str()) {
        (&Method::GET, "/info") => return info(),
        (&Method::PUT | &Method::POST, "/v0.4/traces") => false,
        (&Method::PUT | &Method::POST, "/v0.5/traces") => true,
        // Remote configuration not offered: a tracer polls no more.
        (_, "/v0.7/config") => return srv::text(StatusCode::NOT_FOUND, "remote configuration: not offered"),
        _ => return srv::json(StatusCode::ACCEPTED, &json!({})),
    };
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_owned();
    let (language, container) = (header("datadog-meta-lang"), header("datadog-container-id"));
    let encoding = srv::header_str(&req, &header::CONTENT_ENCODING).unwrap_or("").to_owned();
    let body = match srv::body_bytes(req.into_body(), MAX_BODY + 1).await {
        Ok(b) => b,
        Err(_) => return srv::text(StatusCode::PAYLOAD_TOO_LARGE, "a tracer's payload is taken up to 25 MiB"),
    };
    let (o, now_ns) = (o.clone(), super::clock_ms().saturating_mul(1_000_000));
    let done = crate::rt::blocking(move || {
        // Behind on writes: what the Agent answers when its receiver is over its rate (the tracer
        // drops the payload, and says so).
        if o.busy() {
            return srv::json(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "behind on writes" }));
        }
        let Some((host, conc)) = NODE.get() else { return srv::text(StatusCode::SERVICE_UNAVAILABLE, "observe is starting") };
        let mut plain = o.bytes();
        if let Err(why) = super::ddagent::unzip(&encoding, &body, &mut plain) {
            o.give_bytes(plain);
            return srv::text(StatusCode::BAD_REQUEST, &why);
        }
        let tracer = kiln_obs::ddtrace::Tracer { language: &language, container: &container, host };
        let (mut spans, mut stats) = (kiln_obs::otlp::traces::Spans::default(), Vec::new());
        let decoded = super::ddagent::with_trace_decoder(|d| {
            while d.spares() < 2 {
                let b = o.bytes();
                if b.capacity() == 0 {
                    break;
                }
                d.give_rows(b);
            }
            d.decode_tracer(v5, &plain, &tracer, now_ns, &mut spans, &mut stats)
        });
        let r = match decoded {
            Ok(_) => {
                {
                    let mut c = lock(conc);
                    let now_ms = now_ns.div_euclid(1_000_000);
                    for s in &stats {
                        c.add(s, now_ms);
                    }
                }
                if o.offer_spans(spans.segments) {
                    srv::json(StatusCode::OK, &json!({ "rate_by_service": { "service:,env:": 1 } }))
                } else {
                    srv::json(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "behind on writes" }))
                }
            }
            Err(e) => srv::text(StatusCode::BAD_REQUEST, &e.to_string()),
        };
        drop(stats);
        o.give_bytes(plain);
        r
    })
    .await;
    done.unwrap_or_else(|e| srv::text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))
}
