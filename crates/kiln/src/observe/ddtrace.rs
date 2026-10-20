//! Datadog's tracers, sent to kiln itself (`docs/observe.md` §8.7): each runner's loopback receiver
//! answers the trace agent's own API, and listens on its port too (`127.0.0.1:8126`, unless
//! `KILN_DD_TRACE_ADDR` says another, or `off`), so an app a Datadog tracer traces sends its traces
//! to the machine's kiln as to its Agent, no Agent beside it:
//!
//! - `PUT` or `POST /v0.4/traces` and `/v0.5/traces` (MessagePack): spans
//!   (`kiln_obs::ddtrace::Decoder::decode_tracer`), each counted by this node's concentrator once
//!   they are taken (`kiln_obs::ddtrace::Concentrator`: the trace metrics, of every span, as the
//!   Agent's stats count them; a payload refused, 429, counted as it comes again), a trace its
//!   tracer dropped counted and not kept; answered with each service's sampling rate
//!   (`rate_by_service`), the Agent's priority sampler's (`kiln_obs::ddtrace::Sampler`): some ten
//!   traces a second kept of the node's tracers' (`KILN_DD_TARGET_TPS`; 0, every trace), each
//!   service its share, a tracer's own rules and errors kept besides;
//! - `GET /info`: what it takes, those two alone, so a tracer computes no stats of its own and drops
//!   no trace for them;
//! - its other products' paths (telemetry, remote configuration, profiles, its event platform's
//!   proxy) answered and passed over, so a tracer neither retries nor complains.
//!
//! The concentrator's buckets are written by a task each second once due, as samples any node
//! takes; refused (the node behind), held and offered again the next second. As the node stops,
//! every bucket open is written and waited on (`super::last_written`: the agent lingers for it).

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use http::{Method, StatusCode, header};
use kiln_obs::ddtrace::{Concentrator, Sampler};
use serde_json::json;

use super::Observe;
use crate::http::server as srv;

/// A tracer's request at the most: the trace agent's (`apm_config.max_payload_size`, 25 MiB).
pub const MAX_BODY: usize = 25 << 20;

/// Traces a second this node's tracers keep, unless `KILN_DD_TARGET_TPS` says another: the
/// Agent's (`apm_config.target_traces_per_second`).
pub const TARGET_TPS: f64 = 10.0;

/// This node's: its name (the host its traces' Agent is), its concentrator and its sampler.
static NODE: OnceLock<(String, Mutex<Concentrator>, Mutex<Sampler>)> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The target its sampler keeps: `KILN_DD_TARGET_TPS`, else [`TARGET_TPS`].
fn target_tps() -> f64 {
    std::env::var("KILN_DD_TARGET_TPS").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|t| t.is_finite() && *t >= 0.0).unwrap_or(TARGET_TPS)
}

/// Whether `path` is one of the trace agent's that tracers send to.
pub fn is_tracer(path: &str) -> bool {
    matches!(path, "/info" | "/v0.4/traces" | "/v0.5/traces" | "/v0.6/stats" | "/v0.7/config" | "/dogstatsd/v2/proxy")
        || ["/telemetry/proxy/", "/profiling/", "/evp_proxy/", "/debugger/", "/symdb/"].iter().any(|p| path.starts_with(p))
}

/// Starts the concentrator of this node (`host`) and the task that writes its buckets as they come
/// due, until `shutdown`: then every bucket open, offered until taken (two seconds at the most), what
/// the node owes as it stops (`super::owes_last`).
pub fn start(o: Observe, host: String, shutdown: crate::rt::Shutdown) {
    if NODE.set((host.clone(), Mutex::new(Concentrator::new(&host)), Mutex::new(Sampler::new(target_tps())))).is_err() {
        return;
    }
    let owed = super::owes_last();
    crate::rt::spawn(async move {
        let mut held: Option<kiln_obs::otlp::metrics::Decoded> = None;
        loop {
            let going = !shutdown.sleep(Duration::from_secs(1)).await;
            // What was refused offered again first; nothing more written while it waits (buckets
            // coming due wait in the concentrator, nine at the most).
            if let Some(d) = held.take()
                && let Err(back) = o.offer_samples_or_back(d)
            {
                held = Some(*back);
            }
            if going {
                break;
            }
            if held.is_none()
                && let Some((_, c, _)) = NODE.get()
            {
                let mut d = o.decoding();
                lock(c).flush_into(super::clock_ms(), false, super::otlp::keys(), &mut d);
                if let Err(back) = o.offer_samples_or_back(d) {
                    held = Some(*back);
                }
            }
        }
        // As the node stops: what is held and every bucket open (the agent lingers three seconds
        // for them to be written).
        let mut last: Vec<kiln_obs::otlp::metrics::Decoded> = held.into_iter().collect();
        last.extend(every_bucket(&o));
        offer_last(&o, last, std::time::Instant::now() + Duration::from_secs(2)).await;
        owed.set();
    });
}

/// Every bucket this node's concentrator has open, written into a decoding.
fn every_bucket(o: &Observe) -> Option<kiln_obs::otlp::metrics::Decoded> {
    let (_, c, _) = NODE.get()?;
    let mut d = o.decoding();
    lock(c).flush_into(super::clock_ms(), true, super::otlp::keys(), &mut d);
    Some(d)
}

/// `batches` offered, each until taken (until `until`: a node behind on its writes).
async fn offer_last(o: &Observe, batches: Vec<kiln_obs::otlp::metrics::Decoded>, until: std::time::Instant) {
    for mut d in batches {
        while let Err(back) = o.offer_samples_or_back(d) {
            if std::time::Instant::now() >= until {
                break;
            }
            d = *back;
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// As a runner retires (an upgrade's new build took over, its own jobs done), its process ending
/// with nothing stopped: every bucket open written, and waited on, for `within` at the most.
pub async fn finish(within: Duration) {
    let Some(o) = super::api::get() else { return };
    let until = std::time::Instant::now() + within;
    offer_last(o, every_bucket(o).into_iter().collect(), until).await;
    let written = super::ingest::Ack::new();
    o.settle(written.clone());
    while !written.done() && std::time::Instant::now() < until {
        crate::rt::sleep(Duration::from_millis(50)).await;
    }
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
        let Some((host, conc, sampler)) = NODE.get() else { return srv::text(StatusCode::SERVICE_UNAVAILABLE, "observe is starting") };
        let mut plain = o.bytes();
        if let Err(why) = super::ddagent::unzip(&encoding, &body, &mut plain) {
            o.give_bytes(plain);
            return srv::text(StatusCode::BAD_REQUEST, &why);
        }
        let tracer = kiln_obs::ddtrace::Tracer { language: &language, container: &container, host };
        let (mut spans, mut stats, mut roots) = (kiln_obs::otlp::traces::Spans::default(), Vec::new(), Vec::new());
        let decoded = super::ddagent::with_trace_decoder(|d| {
            while d.spares() < 2 {
                let b = o.bytes();
                if b.capacity() == 0 {
                    break;
                }
                d.give_rows(b);
            }
            d.decode_tracer(v5, &plain, &tracer, now_ns, &mut spans, &mut stats, &mut roots)
        });
        let r = match decoded {
            // Counted once its spans are taken: a payload refused is sent again, and counted then.
            Ok(_) if !o.offer_spans(spans.segments) => srv::json(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "behind on writes" })),
            Ok(_) => {
                let now_ms = now_ns.div_euclid(1_000_000);
                {
                    let mut c = lock(conc);
                    for s in &stats {
                        c.add(s, now_ms);
                    }
                }
                let rates: serde_json::Map<String, serde_json::Value> = {
                    let mut s = lock(sampler);
                    for r in &roots {
                        s.count(r, now_ms);
                    }
                    s.rates().map(|(k, r)| (k, serde_json::Value::from(r))).collect()
                };
                srv::json(StatusCode::OK, &json!({ "rate_by_service": rates }))
            }
            Err(e) => srv::text(StatusCode::BAD_REQUEST, &e.to_string()),
        };
        drop((stats, roots));
        o.give_bytes(plain);
        r
    })
    .await;
    done.unwrap_or_else(|e| srv::text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")))
}
