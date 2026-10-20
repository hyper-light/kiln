//! OpenTelemetry export (docs/ci.md W19-R02, W11-R09): each finished run, as an OTLP trace sent
//! to every `otel` integration its repository may use. The run is the root span; each of its
//! legs (as its latest attempt ran) a span under it; each step a span under its leg. Attributes
//! follow OpenTelemetry's CI/CD and VCS conventions (`cicd.pipeline.*`, `vcs.*`), kiln's own
//! (`kiln.*`) beside them. OTLP/HTTP with JSON, `<endpoint>/v1/traces`: a Collector, Jaeger,
//! Tempo, Honeycomb, or any vendor's OTLP intake.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::integrations::{self, Integration};
use crate::hub::Hub;

/// A finished run, as its trace tells it.
#[derive(Debug, Clone, Default)]
pub struct RunFacts {
    pub id: String,
    pub repo: String,
    pub clone_url: String,
    pub number: i64,
    pub workflow: String,
    pub name: String,
    pub event: String,
    pub branch: String,
    pub sha: String,
    pub actor: String,
    pub conclusion: String,
    pub attempt: i64,
    pub created_at: i64,
    pub finished_at: i64,
    pub url: String,
}

/// One leg of it.
#[derive(Debug, Clone, Default)]
pub struct LegFacts {
    pub id: String,
    pub key: String,
    pub name: String,
    pub conclusion: String,
    pub queued_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub runner: Option<String>,
    pub executor: Option<String>,
    pub steps: Vec<StepFacts>,
}

/// One step of a leg.
#[derive(Debug, Clone, Default)]
pub struct StepFacts {
    pub idx: i64,
    pub name: String,
    pub conclusion: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

/// `n` bytes of SHA-256 over `what`, as lower-case hex: a trace's or span's id that a run's facts
/// always give the same.
fn id_of(what: &str, n: usize) -> String {
    crate::util::sha256_hex(what.as_bytes()).chars().take(n * 2).collect()
}

/// OpenTelemetry's `cicd.pipeline.result` (and a task run's) for a GitHub conclusion.
fn result_of(conclusion: &str) -> &'static str {
    match conclusion {
        "success" => "success",
        "failure" | "startup_failure" => "failure",
        "cancelled" => "cancellation",
        "timed_out" => "timeout",
        "skipped" | "neutral" => "skip",
        _ => "error",
    }
}

/// A span's status: OK for a success, ERROR (with the conclusion) for a failure, else unset.
fn status_of(conclusion: &str) -> Value {
    match result_of(conclusion) {
        "success" => json!({ "code": 1 }),
        "failure" | "timeout" | "error" => json!({ "code": 2, "message": conclusion }),
        _ => json!({ "code": 0 }),
    }
}

fn nanos(secs: i64) -> String {
    (i128::from(secs.max(0)) * 1_000_000_000).to_string()
}

fn attr(key: &str, value: Value) -> Value {
    let v = match value {
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Number(n) if n.is_i64() => json!({ "intValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n.as_f64().unwrap_or_default() }),
        other => json!({ "stringValue": other.as_str().map_or_else(|| other.to_string(), str::to_owned) }),
    };
    json!({ "key": key, "value": v })
}

/// The run's trace as OTLP/JSON (`ExportTraceServiceRequest`): its spans, `service` naming the
/// resource.
pub fn trace(run: &RunFacts, legs: &[LegFacts], service: &str) -> Value {
    let trace_id = id_of(&format!("kiln-run:{}:{}", run.id, run.attempt), 16);
    let root = id_of(&format!("kiln-run-span:{}:{}", run.id, run.attempt), 8);
    let vcs = || {
        vec![
            attr("vcs.repository.name", json!(run.repo)),
            attr("vcs.repository.url.full", json!(run.clone_url)),
            attr("vcs.ref.head.name", json!(run.branch)),
            attr("vcs.ref.head.revision", json!(run.sha)),
        ]
    };
    let mut attributes = vec![
        attr("cicd.pipeline.name", json!(run.name)),
        attr("cicd.pipeline.run.id", json!(run.id)),
        attr("cicd.pipeline.run.url.full", json!(run.url)),
        attr("cicd.pipeline.result", json!(result_of(&run.conclusion))),
        attr("kiln.workflow", json!(run.workflow)),
        attr("kiln.run.number", json!(run.number)),
        attr("kiln.run.attempt", json!(run.attempt)),
        attr("kiln.event", json!(run.event)),
        attr("kiln.actor", json!(run.actor)),
        attr("kiln.conclusion", json!(run.conclusion)),
    ];
    attributes.extend(vcs());
    let mut spans = vec![json!({
        "traceId": trace_id, "spanId": root, "name": run.name, "kind": 1,
        "startTimeUnixNano": nanos(run.created_at), "endTimeUnixNano": nanos(run.finished_at.max(run.created_at)),
        "attributes": attributes, "status": status_of(&run.conclusion),
    })];
    for l in legs {
        // A leg that never started (skipped, cancelled while waiting) spans its wait, if any.
        let start = l.started_at.or(l.queued_at).unwrap_or(run.created_at);
        let end = l.finished_at.unwrap_or(run.finished_at).max(start);
        let span = id_of(&format!("kiln-leg:{}", l.id), 8);
        let mut a = vec![
            attr("cicd.pipeline.task.name", json!(l.name)),
            attr("cicd.pipeline.task.run.id", json!(l.id)),
            attr("cicd.pipeline.task.run.url.full", json!(format!("{}/jobs/{}", run.url, l.id))),
            attr("cicd.pipeline.task.run.result", json!(result_of(&l.conclusion))),
            attr("kiln.leg.key", json!(l.key)),
            attr("kiln.conclusion", json!(l.conclusion)),
        ];
        if let (Some(q), Some(s)) = (l.queued_at, l.started_at) {
            a.push(attr("kiln.queued_seconds", json!(s.saturating_sub(q).max(0))));
        }
        if let Some(r) = &l.runner {
            a.push(attr("cicd.worker.name", json!(r)));
        }
        if let Some(e) = &l.executor {
            a.push(attr("kiln.executor", json!(e)));
        }
        spans.push(json!({
            "traceId": trace_id, "spanId": span, "parentSpanId": root, "name": l.name, "kind": 1,
            "startTimeUnixNano": nanos(start), "endTimeUnixNano": nanos(end), "attributes": a, "status": status_of(&l.conclusion),
        }));
        for s in &l.steps {
            let (Some(from), Some(to)) = (s.started_at, s.finished_at) else { continue };
            spans.push(json!({
                "traceId": trace_id, "spanId": id_of(&format!("kiln-step:{}:{}", l.id, s.idx), 8), "parentSpanId": span, "name": s.name, "kind": 1,
                "startTimeUnixNano": nanos(from), "endTimeUnixNano": nanos(to.max(from)),
                "attributes": [attr("kiln.step.index", json!(s.idx)), attr("kiln.conclusion", json!(s.conclusion))], "status": status_of(&s.conclusion),
            }));
        }
    }
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [attr("service.name", json!(service)), attr("service.version", json!(env!("CARGO_PKG_VERSION")))] },
            "scopeSpans": [{ "scope": { "name": "kiln.ci", "version": env!("CARGO_PKG_VERSION") }, "spans": spans }],
        }],
    })
}

/// A run's facts, and its latest attempt's legs with their steps.
async fn facts(hub: &Hub, run: &str) -> anyhow::Result<Option<(String, RunFacts, Vec<LegFacts>)>> {
    let id = run.to_owned();
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    hub.store
        .call(move |c| {
            let row = c
                .query_row(
                    "SELECT p.id, p.name, p.clone_url, r.number, r.pipeline, COALESCE(r.run_name, ''), r.event, r.branch, r.sha, r.actor, COALESCE(r.conclusion, r.status), r.attempt, r.created_at, COALESCE(r.finished_at, r.created_at)
                     FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1",
                    [&id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            RunFacts {
                                id: id.clone(),
                                repo: r.get(1)?,
                                clone_url: r.get(2)?,
                                number: r.get(3)?,
                                workflow: r.get(4)?,
                                name: r.get(5)?,
                                event: r.get(6)?,
                                branch: r.get(7)?,
                                sha: r.get(8)?,
                                actor: r.get(9)?,
                                conclusion: r.get(10)?,
                                attempt: r.get(11)?,
                                created_at: r.get(12)?,
                                finished_at: r.get(13)?,
                                url: format!("{base}/runs/{id}"),
                            },
                        ))
                    },
                )
                .optional()?;
            let Some((repo_id, mut run)) = row else { return Ok(None) };
            if run.name.is_empty() {
                run.name = run.workflow.rsplit('/').next().unwrap_or(&run.workflow).to_owned();
            }
            let mut st = c.prepare(
                "SELECT id, key, display, COALESCE(conclusion, status), queued_at, started_at, finished_at, agent, executor FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = ?2 ORDER BY COALESCE(started_at, queued_at), key",
            )?;
            let mut legs: Vec<LegFacts> = st
                .query_map(params![run.id, run.attempt], |r| {
                    Ok(LegFacts {
                        id: r.get(0)?,
                        key: r.get(1)?,
                        name: r.get(2)?,
                        conclusion: r.get(3)?,
                        queued_at: r.get(4)?,
                        started_at: r.get(5)?,
                        finished_at: r.get(6)?,
                        runner: r.get(7)?,
                        executor: r.get(8)?,
                        steps: Vec::new(),
                    })
                })?
                .collect::<Result<_, _>>()?;
            let mut st = c.prepare("SELECT idx, name, COALESCE(conclusion, status), started_at, finished_at FROM steps WHERE job_id = ?1 ORDER BY idx")?;
            for l in &mut legs {
                l.steps = st
                    .query_map([&l.id], |r| Ok(StepFacts { idx: r.get(0)?, name: r.get(1)?, conclusion: r.get(2)?, started_at: r.get(3)?, finished_at: r.get(4)? }))?
                    .collect::<Result<_, _>>()?;
            }
            Ok(Some((repo_id, run, legs)))
        })
        .await
}

/// Where an integration's traces go: its endpoint's `/v1/traces` (unless it names that already).
fn traces_url(i: &Integration) -> Result<String, String> {
    let e = i.config.get("endpoint").and_then(Value::as_str).map(str::trim).filter(|e| !e.is_empty()).ok_or("an otel integration needs its `endpoint`")?;
    let e = e.trim_end_matches('/');
    Ok(if e.ends_with("/v1/traces") { e.to_owned() } else { format!("{e}/v1/traces") })
}

/// Sends `body` to `i`'s collector: with its stored header (`Name: value`), only where requests
/// someone named may go (W22-AC07). What it answered.
async fn post(hub: &Hub, i: &Integration, body: &Value) -> Result<String, String> {
    let url = traces_url(i)?;
    let header = integrations::token(hub, i).await.map_err(|e| format!("{e:#}"))?;
    let client = crate::http::client::Client::public().map_err(|e| format!("{e:#}"))?;
    let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let mut req = client.post(&url).header("content-type", "application/json").body(payload).timeout(Duration::from_secs(20)).reaching(&hub.cfg.ci.reach());
    if let Some(h) = header.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        let (name, value) = h.split_once(':').ok_or("its stored header is `Name: value`")?;
        req = req.header(name.trim(), value.trim());
    }
    // On the heap: the request's future (the reach check, TLS) is deep.
    let resp = Box::pin(req.send()).await.map_err(|e| format!("{e:#}"))?;
    let status = resp.status();
    let answer = String::from_utf8_lossy(&resp.bytes(16 << 10).await.unwrap_or_default()).into_owned();
    if (200..300).contains(&status) { Ok(format!("{url} took it ({status})")) } else { Err(format!("{url} answered {status}: {}", answer.chars().take(300).collect::<String>())) }
}

/// A run ended: its trace, to every `otel` integration its repository may use.
pub async fn run_finished(hub: Arc<Hub>, run: String) {
    let all = match integrations::list(&hub).await {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!(run = %run, error = %format!("{e:#}"), "integrations couldn't be read: no trace");
            return;
        }
    };
    if !all.iter().any(|i| i.kind == "otel") {
        return;
    }
    let found = match facts(&hub, &run).await {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(run = %run, error = %format!("{e:#}"), "the run couldn't be read: no trace");
            return;
        }
    };
    let (repo_id, facts, legs) = found;
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|x| x.id == repo_id)) else { return };
    for i in all.iter().filter(|i| i.kind == "otel" && integrations::usable_by(i, &repo)) {
        let service = i.config.get("service_name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("kiln-ci");
        let result = post(&hub, i, &trace(&facts, &legs, service)).await;
        let (id, err) = (i.id.clone(), result.as_ref().err().cloned());
        if let Some(e) = &err {
            tracing::warn!(integration = %i.name, run = %run, error = %e, "a run's trace wasn't taken");
        }
        let _ = hub
            .store
            .call(move |c| Ok(c.execute("UPDATE ci_integrations SET last_used_at = ?2, last_error = ?3 WHERE id = ?1", params![id, crate::util::now_unix(), err])?))
            .await;
    }
}

/// Verify: an empty export, which a collector takes.
pub async fn verify(hub: &Hub, i: &Integration) -> Result<String, String> {
    post(hub, i, &json!({ "resourceSpans": [] })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W19-R02: a run's trace is OTLP's: one trace for the attempt, the run the root, each leg
    /// under it and each step under its leg, with the CI/CD conventions' attributes and status.
    #[test]
    fn w19_r02_a_run_as_a_trace() {
        let run = RunFacts {
            id: "r1".into(),
            repo: "acme/app".into(),
            number: 7,
            workflow: ".github/workflows/ci.yml".into(),
            name: "ci".into(),
            conclusion: "failure".into(),
            attempt: 2,
            created_at: 100,
            finished_at: 160,
            url: "https://kiln.example/runs/r1".into(),
            ..RunFacts::default()
        };
        let steps = vec![
            StepFacts { idx: 0, name: "checkout".into(), conclusion: "success".into(), started_at: Some(110), finished_at: Some(112) },
            StepFacts { idx: 1, name: "test".into(), conclusion: "failure".into(), started_at: Some(112), finished_at: Some(150) },
            StepFacts { idx: 2, name: "never ran".into(), conclusion: "skipped".into(), started_at: None, finished_at: None },
        ];
        let legs = vec![
            LegFacts { id: "l1".into(), key: "test".into(), name: "test".into(), conclusion: "failure".into(), queued_at: Some(101), started_at: Some(105), finished_at: Some(155), runner: Some("box".into()), executor: Some("docker".into()), steps },
            LegFacts { id: "l2".into(), key: "deploy".into(), name: "deploy".into(), conclusion: "skipped".into(), ..LegFacts::default() },
        ];
        let t = trace(&run, &legs, "kiln-ci");
        let spans = t.pointer("/resourceSpans/0/scopeSpans/0/spans").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(spans.len(), 5, "the run, two legs, two steps that ran: {t}");
        let span = |i: usize| spans.get(i).cloned().unwrap_or_default();
        let ids: Vec<&str> = spans.iter().filter_map(|s| s.get("traceId").and_then(Value::as_str)).collect();
        assert!(ids.iter().all(|i| i.len() == 32 && Some(i) == ids.first()), "one trace: {ids:?}");
        let first = span(0);
        let root = first.get("spanId").and_then(Value::as_str).unwrap_or_default();
        assert_eq!(root.len(), 16);
        assert!(span(0).get("parentSpanId").is_none());
        assert_eq!(span(1).get("parentSpanId").and_then(Value::as_str), Some(root), "the leg is the run's");
        assert_eq!(span(2).get("parentSpanId"), span(1).get("spanId"), "the step is its leg's");
        assert_eq!(span(0).pointer("/status/code"), Some(&json!(2)), "a failure is an error");
        assert_eq!(span(0).get("startTimeUnixNano"), Some(&json!("100000000000")));
        let has = |s: &Value, k: &str, v: &str| s.get("attributes").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.get("key") == Some(&json!(k)) && x.pointer("/value/stringValue") == Some(&json!(v))));
        assert!(has(&span(0), "cicd.pipeline.result", "failure") && has(&span(0), "vcs.repository.name", "acme/app"));
        assert!(has(&span(1), "cicd.worker.name", "box") && has(&span(1), "cicd.pipeline.task.run.url.full", "https://kiln.example/runs/r1/jobs/l1"));
        assert!(has(&span(4), "cicd.pipeline.task.run.result", "skip"), "a leg that never ran is there");
        assert_eq!(span(4).pointer("/status/code"), Some(&json!(0)));
        // The same run's next attempt is another trace.
        let again = trace(&RunFacts { attempt: 3, ..run }, &legs, "kiln-ci");
        assert_ne!(again.pointer("/resourceSpans/0/scopeSpans/0/spans/0/traceId"), t.pointer("/resourceSpans/0/scopeSpans/0/spans/0/traceId"));
        assert_eq!(t.pointer("/resourceSpans/0/resource/attributes/0/value/stringValue"), Some(&json!("kiln-ci")));
    }
}
