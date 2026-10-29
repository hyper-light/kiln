//! The catalog's API (`docs/observe.md` §10.8): Datadog's at its paths for what is defined
//! (`POST /api/v2/services/definitions`, JSON or YAML; `DELETE` one), with the ingest token; and
//! kiln's (`/api/observe/v1/catalog`, a session's): every service with its facts and scores, one
//! service, a definition made or removed.

use std::collections::BTreeMap;

use http::StatusCode;
use serde_json::{Value, json};

use super::{Facts, Kept, PutError, Source, definition, rules};
use crate::http::server::{self as srv, Request, Response};
use crate::observe::api::{Fail, fail};

/// A body, at the most.
const BODY: usize = 1 << 20;

fn catalog() -> Result<&'static std::sync::Arc<super::Catalog>, Fail> {
    super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "the catalog is not open"))
}

fn rfc3339(ms: i64) -> String {
    i128::from(ms)
        .checked_mul(1_000_000)
        .and_then(|ns| time::OffsetDateTime::from_unix_timestamp_nanos(ns).ok())
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

/// A definition as Datadog's API shows it.
fn definition_json(k: &Kept) -> Value {
    let (source, detail) = match &k.source {
        Source::Api => ("api", String::new()),
        Source::Repo { repo, file, sha } => ("repository", format!("{repo}:{file}@{}", sha.get(..12).unwrap_or(sha))),
    };
    json!({
        "id": k.definition.dd_service,
        "type": "service-definition",
        "attributes": {
            "schema": k.definition,
            "meta": { "ingested-schema-version": k.definition.schema_version, "ingestion-source": source, "origin": source, "origin-detail": detail, "last-modified-time": rfc3339(k.modified), "warnings": [] },
        }
    })
}

/// The definitions a body holds (JSON or YAML), each made or changed.
async fn put(req: Request) -> Result<Response, Fail> {
    let body = srv::body_bytes(req.into_body(), BODY).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let defs = definition::parse(&String::from_utf8_lossy(&body)).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let c = catalog()?;
    let mut kept = Vec::new();
    for d in defs {
        kept.push(c.put(d).map_err(|e| match e {
            PutError::Invalid(_) => fail(StatusCode::BAD_REQUEST, e),
            PutError::InRepo(_) => fail(StatusCode::CONFLICT, e),
            PutError::Unsaved(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, e),
        })?);
    }
    Ok(srv::json(StatusCode::OK, &json!({ "data": kept.iter().map(definition_json).collect::<Vec<_>>() })))
}

async fn remove(name: &str) -> Result<Response, Fail> {
    match catalog()?.remove(name) {
        Ok(true) => Ok(srv::status(StatusCode::NO_CONTENT)),
        Ok(false) => Err(fail(StatusCode::NOT_FOUND, "no such definition")),
        Err(e @ PutError::InRepo(_)) => Err(fail(StatusCode::CONFLICT, e)),
        Err(e) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// Datadog's service definition API at its paths (`/api/v2/services/definitions...`), the ingest
/// token found good: what is defined.
pub async fn datadog(path: &[&str], req: Request) -> Response {
    let method = req.method().clone();
    let answer = match (&method, path) {
        (&http::Method::POST, []) => put(req).await,
        (&http::Method::DELETE, [name]) => remove(name).await,
        (&http::Method::GET, _) => Err(fail(StatusCode::FORBIDDEN, "the catalog is read through kiln's API (/api/observe/v1/catalog), with a session")),
        _ => Err(fail(StatusCode::NOT_FOUND, "no such service definition endpoint")),
    };
    answer.unwrap_or_else(|(code, why)| srv::json(code, &json!({ "errors": [why] })))
}

/// A service's scorecards: each one's rules passed of its rules.
fn scores(rs: &[super::Rule]) -> Value {
    let mut by: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for r in rs {
        let e = by.entry(r.scorecard).or_default();
        e.1 += 1;
        if r.passed {
            e.0 += 1;
        }
    }
    let passed = rs.iter().filter(|r| r.passed).count();
    json!({
        "passed": passed,
        "total": rs.len(),
        "scorecards": by.iter().map(|(name, (p, t))| json!({ "name": name, "passed": p, "total": t })).collect::<Vec<_>>(),
    })
}

/// A service as the list shows it.
fn entry(name: &str, k: Option<&Kept>, f: &Facts, now: i64) -> Value {
    let d = k.map(|k| &k.definition);
    let rs = rules(k, f, now);
    let (source, repo) = match k.map(|k| &k.source) {
        None => ("seen", None),
        Some(Source::Api) => ("api", None),
        Some(Source::Repo { repo, .. }) => ("repository", Some(repo.clone())),
    };
    json!({
        "service": name,
        "source": source,
        "repo": repo,
        "team": d.and_then(|d| d.team.clone()),
        "tier": d.and_then(|d| d.tier.clone()),
        "lifecycle": d.and_then(|d| d.lifecycle.clone()),
        "type": d.and_then(|d| d.kind),
        "description": d.and_then(|d| d.description.clone()),
        "requests": f.requests,
        "errors": f.errors,
        "error_rate": (f.requests > 0.0).then(|| f.errors / f.requests),
        "p95_s": f.p95_s,
        "open_issues": f.open_issues,
        "monitors": f.monitors.len(),
        "alerting": f.monitors.iter().filter(|m| m.state != "OK" && m.state != "No Data").count(),
        "slos": f.slos.len(),
        "deployments_30d": f.deployments_30d,
        "failed_30d": f.failed_30d,
        "last_deployment": f.last_deployment,
        "score": scores(&rs),
    })
}

/// `GET catalog`: every service, defined or seen, with its facts and scores.
pub async fn services() -> Result<Response, Fail> {
    let now = crate::observe::clock_ms();
    let all = catalog()?.services(now).await;
    let services: Vec<Value> = all.iter().map(|(name, k, f)| entry(name, k.as_ref(), f, now)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "services": services })))
}

/// `GET catalog/{service}`: one, its definition, every rule, the monitors and SLOs that read it,
/// its latest deployments.
pub async fn service(name: &str) -> Result<Response, Fail> {
    let now = crate::observe::clock_ms();
    let (k, f) = catalog()?.service(name, now).await.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such service: none defines it, nor is it seen"))?;
    let mut v = entry(name, k.as_ref(), &f, now);
    let deployments: Vec<Value> = crate::observe::dora::get()
        .map(|d| {
            d.read(|l| {
                l.latest_of(name, 5)
                    .into_iter()
                    .map(|r| json!({ "id": r.id(), "env": r.env(), "version": r.version(), "finished_at": r.finished_at() / 1_000_000, "failed": r.change_failure().is_some() }))
                    .collect()
            })
        })
        .unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("definition".into(), k.as_ref().map_or(Value::Null, definition_json));
        obj.insert("rules".into(), json!(rules(k.as_ref(), &f, now)));
        obj.insert("monitor_list".into(), json!(f.monitors));
        obj.insert("slo_list".into(), json!(f.slos));
        obj.insert("deployments".into(), json!(deployments));
        obj.insert("traced".into(), json!(f.traced));
        obj.insert("logs_traced".into(), json!(f.logs_traced));
    }
    Ok(srv::json(StatusCode::OK, &v))
}

/// `POST catalog/definitions`: definitions made or changed (JSON or YAML, as Datadog's API takes them).
pub async fn define(req: Request) -> Result<Response, Fail> {
    put(req).await
}

/// `DELETE catalog/definitions/{service}`.
pub async fn undefine(name: &str) -> Result<Response, Fail> {
    remove(name).await
}
