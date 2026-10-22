//! DORA's API (`docs/observe.md` §10.7): Datadog's at its own paths, what a CD system sends with
//! the ingest token (`POST /api/v2/dora/deployment` and `/failure`, `PATCH` a deployment as failed,
//! `DELETE` one), and kiln's (`/api/observe/v1/dora/...`, a session's): the same and the reading,
//! lists of deployments and failures, one of each, and the four keys for the page.

use std::collections::HashMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ledger::{DAY_NS, Query};
use super::{Deployment, Dora, Failure, Git, PatchError, RemedyBy, RemedyKind, git, levels};
use crate::http::server::{self as srv, Request, Response};
use crate::observe::api::{Fail, fail, read_json};

/// A request's body, at the most.
const BODY: usize = 1 << 20;
/// Entries a list gives, at the most (Datadog's), and unless told.
const LIST_MAX: usize = 1_000;
const LIST_DEFAULT: usize = 100;
/// How far ahead of this clock a deployment may say it finished (another's clock).
const AHEAD_NS: i64 = 10 * 60 * 1_000_000_000;
/// Tags an event takes, at the most (Datadog's).
const TAGS_MAX: usize = 100;
/// A name's length (a service's, an environment's, a team's, a version's), at the most.
const NAME_MAX: usize = 256;

fn dora() -> Result<&'static Arc<Dora>, Fail> {
    super::get().ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "DORA's ledger is not open"))
}

#[derive(Debug, Deserialize)]
pub struct GitIn {
    commit_sha: String,
    repository_url: String,
}

/// A deployment event as Datadog's API takes it.
#[derive(Debug, Deserialize)]
pub struct DeploymentIn {
    service: String,
    #[serde(default)]
    env: Option<String>,
    started_at: Value,
    finished_at: Value,
    #[serde(default)]
    git: Option<GitIn>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    custom_tags: Option<Vec<String>>,
}

/// A failure (incident) event as Datadog's API takes it.
#[derive(Debug, Deserialize)]
pub struct FailureIn {
    #[serde(default)]
    services: Option<Vec<String>>,
    #[serde(default)]
    env: Option<String>,
    started_at: Value,
    #[serde(default)]
    finished_at: Option<Value>,
    #[serde(default)]
    git: Option<GitIn>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    custom_tags: Option<Vec<String>>,
}

/// A deployment marked failed (or not), as Datadog's `PATCH` takes it: by id in its path, or by its
/// service, environment and version.
#[derive(Debug, Deserialize)]
pub struct PatchIn {
    #[serde(default)]
    change_failure: Option<bool>,
    #[serde(default)]
    remediation: Option<RemediationIn>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemediationIn {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<RemedyKind>,
}

/// A time as Datadog takes it, seconds, milliseconds or nanoseconds since the epoch (told apart by
/// size, each from 2001 on), in ns.
pub fn time_ns(v: &Value, what: &str) -> Result<i64, String> {
    let n = v
        .as_i64()
        .or_else(|| {
            #[allow(clippy::cast_possible_truncation)]
            v.as_f64().filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18).map(|f| f as i64)
        })
        .ok_or_else(|| format!("{what}: a whole number of seconds, milliseconds or nanoseconds since the epoch"))?;
    match n {
        1_000_000_000..100_000_000_000 => Ok(n.saturating_mul(1_000_000_000)),
        1_000_000_000_000..100_000_000_000_000 => Ok(n.saturating_mul(1_000_000)),
        1_000_000_000_000_000_000..=i64::MAX => Ok(n),
        _ => Err(format!("{what}: {n} is neither seconds, milliseconds nor nanoseconds since the epoch (from 2001)")),
    }
}

/// An id given (16 to 128 of letters, digits, `-`, `_`, `.` and `:`, Datadog's), or one made.
fn id_of(given: Option<String>) -> Result<String, String> {
    match given.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
        Some(id) if (16..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')) => Ok(id),
        Some(id) => Err(format!("id {id:?}: 16 to 128 letters, digits, -, _, . and :")),
        None => Ok(format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>())),
    }
}

fn name_of(v: Option<String>, what: &str) -> Result<String, String> {
    let v = v.map(|s| s.trim().to_owned()).unwrap_or_default();
    if v.len() > NAME_MAX {
        return Err(format!("{what}: {NAME_MAX} bytes at the most"));
    }
    Ok(v)
}

fn tags_of(v: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let tags: Vec<String> = v.unwrap_or_default().into_iter().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect();
    if tags.len() > TAGS_MAX {
        return Err(format!("custom_tags: {TAGS_MAX} at the most"));
    }
    if let Some(t) = tags.iter().find(|t| t.split_once(':').is_none_or(|(k, _)| k.trim().is_empty()) || t.len() > 200) {
        return Err(format!("custom_tags: {t:?} is no key:value tag"));
    }
    Ok(tags)
}

fn git_of(g: Option<GitIn>) -> Result<Option<Git>, String> {
    let Some(g) = g else { return Ok(None) };
    let sha = g.commit_sha.trim().to_ascii_lowercase();
    if !git::is_sha(&sha) {
        return Err(format!("git.commit_sha {:?}: a commit's sha", g.commit_sha));
    }
    let url = g.repository_url.trim().to_owned();
    if url.is_empty() || url.len() > 2048 {
        return Err("git.repository_url: a repository's URL".into());
    }
    Ok(Some(Git { commit_sha: sha, repository_url: url }))
}

/// A deployment event checked: what the ledger keeps of it, or why not.
pub fn deployment_of(a: DeploymentIn, source: &str, now_ns: i64) -> Result<Deployment, String> {
    let service = name_of(Some(a.service), "service")?;
    if service.is_empty() {
        return Err("service: required".into());
    }
    let started_at = time_ns(&a.started_at, "started_at")?;
    let finished_at = time_ns(&a.finished_at, "finished_at")?;
    if started_at > finished_at {
        return Err("started_at: after finished_at".into());
    }
    if finished_at > now_ns.saturating_add(AHEAD_NS) {
        return Err("finished_at: in the future".into());
    }
    Ok(Deployment {
        id: id_of(a.id)?,
        service,
        env: name_of(a.env, "env")?,
        team: name_of(a.team, "team")?,
        version: name_of(a.version, "version")?,
        started_at,
        finished_at,
        git: git_of(a.git)?,
        custom_tags: tags_of(a.custom_tags)?,
        source: source.to_owned(),
        lead: None,
        change_failure: None,
        remedies: Vec::new(),
        shas: Vec::new(),
        titles: Vec::new(),
    })
}

/// A failure event checked: what the ledger keeps of it, or why not.
pub fn failure_of(a: FailureIn, now_ns: i64) -> Result<Failure, String> {
    let services: Vec<String> = a.services.unwrap_or_default().into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
    if services.iter().any(|s| s.len() > NAME_MAX) || services.len() > TAGS_MAX {
        return Err(format!("services: {TAGS_MAX} of {NAME_MAX} bytes at the most"));
    }
    let team = name_of(a.team, "team")?;
    if services.is_empty() && team.is_empty() {
        return Err("services or team: one is required".into());
    }
    let started_at = time_ns(&a.started_at, "started_at")?;
    let finished_at = a.finished_at.as_ref().filter(|v| !v.is_null()).map(|v| time_ns(v, "finished_at")).transpose()?;
    if finished_at.is_some_and(|f| f < started_at) {
        return Err("finished_at: before started_at".into());
    }
    if started_at > now_ns.saturating_add(AHEAD_NS) {
        return Err("started_at: in the future".into());
    }
    Ok(Failure {
        id: id_of(a.id)?,
        services,
        env: name_of(a.env, "env")?,
        team,
        name: name_of(a.name, "name")?,
        severity: name_of(a.severity, "severity")?,
        started_at,
        finished_at,
        git: git_of(a.git)?,
        version: name_of(a.version, "version")?,
        custom_tags: tags_of(a.custom_tags)?,
    })
}

fn ms(ns: i64) -> i64 {
    ns / 1_000_000
}

/// A deployment as the API shows it (Datadog's shape; times in ms, as kiln's API gives them).
pub fn deployment_json(d: &Deployment) -> Value {
    #[allow(clippy::cast_precision_loss)]
    let recovery_s = d.recovery_ns().map(|ns| ns as f64 / 1e9);
    json!({
        "id": d.id,
        "type": "dora_deployment",
        "attributes": {
            "service": d.service,
            "env": d.env,
            "team": d.team,
            "version": d.version,
            "started_at": ms(d.started_at),
            "finished_at": ms(d.finished_at),
            "git": d.git,
            "custom_tags": d.custom_tags,
            "source": d.source,
            "lead": d.lead,
            "change_failure": d.change_failure.as_ref().map(|cf| json!({
                "found": cf.found,
                "remediation": cf.remedy.as_ref().map(|r| json!({ "id": r.id, "type": r.kind, "finished_at": ms(r.finished_at) })),
            })),
            "recovery_s": recovery_s,
            "remedies": d.remedies,
        }
    })
}

/// A failure as the API shows it.
pub fn failure_json(f: &Failure) -> Value {
    json!({
        "id": f.id,
        "type": "dora_failure",
        "attributes": {
            "services": f.services,
            "env": f.env,
            "team": f.team,
            "name": f.name,
            "severity": f.severity,
            "started_at": ms(f.started_at),
            "finished_at": f.finished_at.map(ms),
            "git": f.git,
            "version": f.version,
            "custom_tags": f.custom_tags,
        }
    })
}

/// A body as Datadog's envelope wraps it (`{"data": {"attributes": ...}}`), or bare.
fn unwrap<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Fail> {
    let inner = match v.get("data").and_then(|d| d.get("attributes")) {
        Some(a) => a.clone(),
        None => v,
    };
    serde_json::from_value(inner).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

pub async fn take_deployment(req: Request, source: &str) -> Result<Response, Fail> {
    let a: DeploymentIn = unwrap(read_json(req.into_body(), BODY).await?)?;
    let d = deployment_of(a, source, super::now_ns()).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let id = d.id.clone();
    dora()?.deploy(d, false).await;
    Ok(srv::json(StatusCode::OK, &json!({ "data": { "id": id, "type": "dora_deployment" } })))
}

pub async fn take_failure(req: Request) -> Result<Response, Fail> {
    let a: FailureIn = unwrap(read_json(req.into_body(), BODY).await?)?;
    let f = failure_of(a, super::now_ns()).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let f = dora()?.fail(f).await;
    Ok(srv::json(StatusCode::OK, &json!({ "data": { "id": f.id, "type": "dora_failure" } })))
}

/// A deployment marked failed (or not): `id`, or the deployment `p` names by its service,
/// environment and version (the latest of that version).
pub async fn mark(id: Option<&str>, req: Request) -> Result<Response, Fail> {
    let p: PatchIn = unwrap(read_json(req.into_body(), BODY).await?)?;
    let d = dora()?;
    let id = match id {
        Some(id) => id.to_owned(),
        None => {
            let (service, version) = (p.service.as_deref().map(str::trim).unwrap_or_default(), p.version.as_deref().map(str::trim).unwrap_or_default());
            if service.is_empty() || version.is_empty() {
                return Err(fail(StatusCode::BAD_REQUEST, "a deployment by its id, or its service and version"));
            }
            let env = p.env.as_deref().map(str::trim).unwrap_or_default();
            d.read(|l| l.deployments().filter(|x| x.service == service && x.env == env && x.version == version).max_by_key(|x| (x.finished_at, x.id.clone())).map(|x| x.id.clone()))
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no deployment of that service, environment and version"))?
        }
    };
    let remedy = match p.remediation {
        None => None,
        Some(r) => {
            let kind = r.kind.ok_or_else(|| fail(StatusCode::BAD_REQUEST, "remediation.type: rollback or rollforward"))?;
            let (rid, version) = (r.id.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()), r.version.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
            if rid.is_some() == version.is_some() {
                return Err(fail(StatusCode::BAD_REQUEST, "remediation: its id or its version, one of them"));
            }
            Some(RemedyBy { id: rid, version, kind })
        }
    };
    let failed = p.change_failure.unwrap_or(true);
    if !failed && remedy.is_some() {
        return Err(fail(StatusCode::BAD_REQUEST, "a remediation is of a change failure"));
    }
    match d.patch(&id, failed, remedy).await {
        Ok(now) => Ok(srv::json(StatusCode::OK, &json!({ "data": deployment_json(&now) }))),
        Err(PatchError::NoDeployment) => Err(fail(StatusCode::NOT_FOUND, PatchError::NoDeployment)),
        Err(e) => Err(fail(StatusCode::BAD_REQUEST, e)),
    }
}

/// Datadog's DORA API at its paths (`/api/v2/dora/...`), the ingest token already found good: what
/// a CD system sends.
pub async fn datadog(path: &[&str], req: Request) -> Response {
    let method = req.method().clone();
    let answer = match (&method, path) {
        (&Method::POST, ["deployment"]) => take_deployment(req, "api").await,
        (&Method::POST, ["failure" | "incident"]) => take_failure(req).await,
        (&Method::PATCH, ["deployments", id]) => mark(Some(*id), req).await,
        (&Method::PATCH, ["deployments"]) => mark(None, req).await,
        (&Method::DELETE, ["deployment", id]) => remove(dora().map(|d| d.remove_deployment(id)), "deployment").await,
        (&Method::DELETE, ["failure", id]) => remove(dora().map(|d| d.remove_failure(id)), "failure").await,
        (_, ["deployments" | "failures", ..]) => Err(fail(StatusCode::FORBIDDEN, "DORA's lists are read through kiln's API (/api/observe/v1/dora), with a session")),
        _ => Err(fail(StatusCode::NOT_FOUND, "no such DORA endpoint")),
    };
    answer.unwrap_or_else(|(code, why)| srv::json(code, &json!({ "errors": [why] })))
}

async fn remove(gone: Result<impl std::future::Future<Output = bool>, Fail>, what: &str) -> Result<Response, Fail> {
    if gone?.await { Ok(srv::status(StatusCode::NO_CONTENT)) } else { Err(fail(StatusCode::NOT_FOUND, format!("no such {what}"))) }
}

/// What a list asks: a range (RFC 3339, or seconds, milliseconds or nanoseconds; of deployments'
/// finish, failures' start), a search of the entries' fields, how many, and their order
/// (`-finished_at`, the newest first, unless said).
#[derive(Debug, Default, Deserialize)]
struct ListIn {
    #[serde(default)]
    from: Option<Value>,
    #[serde(default)]
    to: Option<Value>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    sort: Option<String>,
}

fn instant(v: &Value, what: &str) -> Result<i64, Fail> {
    if let Some(s) = v.as_str() {
        let t = time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{what}: {e}")))?;
        return Ok(i64::try_from(t.unix_timestamp_nanos()).unwrap_or(i64::MAX));
    }
    time_ns(v, what).map_err(|e| fail(StatusCode::BAD_REQUEST, e))
}

/// A list's range, search, size and order.
fn list_of(l: ListIn) -> Result<(i64, i64, Option<kiln_obs::logs::search::Search>, usize, bool, bool), Fail> {
    let to = l.to.as_ref().map(|v| instant(v, "to")).transpose()?.unwrap_or_else(|| super::now_ns().saturating_add(AHEAD_NS));
    let from = l.from.as_ref().map(|v| instant(v, "from")).transpose()?.unwrap_or(to.saturating_sub(30 * DAY_NS));
    let q = l.query.as_deref().map(str::trim).filter(|q| !q.is_empty() && *q != "*");
    let search = q.map(kiln_obs::logs::search::parse).transpose().map_err(|e| fail(StatusCode::BAD_REQUEST, format!("query: {e}")))?;
    let limit = l.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let sort = l.sort.as_deref().map(str::trim).unwrap_or("-finished_at");
    let (desc, by_start) = match sort {
        "-finished_at" | "-timestamp" => (true, false),
        "finished_at" | "timestamp" => (false, false),
        "-started_at" => (true, true),
        "started_at" => (false, true),
        other => return Err(fail(StatusCode::BAD_REQUEST, format!("sort {other:?}: finished_at or started_at, - first for the newest first"))),
    };
    Ok((from, to, search, limit, desc, by_start))
}

fn list_deployments(d: &Dora, l: ListIn) -> Result<Response, Fail> {
    let (from, to, search, limit, desc, by_start) = list_of(l)?;
    let mut s = kiln_obs::logs::search::RowScratch::default();
    let data: Vec<Value> = d.read(|l| {
        let mut picked: Vec<&Deployment> = l.finished_between(from, to).filter(|x| search.as_ref().is_none_or(|q| q.fields(*x, &mut s))).collect();
        picked.sort_by_key(|x| (if by_start { x.started_at } else { x.finished_at }, x.id.clone()));
        if desc {
            picked.reverse();
        }
        picked.into_iter().take(limit).map(deployment_json).collect()
    });
    Ok(srv::json(StatusCode::OK, &json!({ "data": data })))
}

fn list_failures(d: &Dora, l: ListIn) -> Result<Response, Fail> {
    let (from, to, search, limit, desc, _) = list_of(l)?;
    let mut s = kiln_obs::logs::search::RowScratch::default();
    let data: Vec<Value> = d.read(|l| {
        let mut picked: Vec<&Failure> = l.failures().filter(|f| f.started_at >= from && f.started_at < to && search.as_ref().is_none_or(|q| q.fields(*f, &mut s))).collect();
        picked.sort_by_key(|f| (f.started_at, f.id.clone()));
        if desc {
            picked.reverse();
        }
        picked.into_iter().take(limit).map(failure_json).collect()
    });
    Ok(srv::json(StatusCode::OK, &json!({ "data": data })))
}

fn param(q: &HashMap<String, String>, k: &str) -> Option<String> {
    q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// The four keys over `from`..`to` (ms; the last 30 days unless said), a service, an environment and
/// a team when given; each beside the 2024 report's levels.
fn summary(d: &Dora, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let ms_of = |k: &str| q.get(k).map(|v| v.trim().parse::<i64>().map_err(|_| fail(StatusCode::BAD_REQUEST, format!("{k}: ms since the epoch")))).transpose();
    let to = ms_of("to")?.map_or_else(super::now_ns, |m| m.saturating_mul(1_000_000));
    let from = ms_of("from")?.map_or(to.saturating_sub(30 * DAY_NS), |m| m.saturating_mul(1_000_000));
    if from >= to {
        return Err(fail(StatusCode::BAD_REQUEST, "from: before to"));
    }
    let query = Query { from, to, service: param(q, "service"), env: param(q, "env"), team: param(q, "team") };
    let (s, (services, envs, teams)) = d.read(|l| (l.summary(&query), l.facets()));
    let weeks: Vec<Value> =
        s.weeks.iter().map(|w| json!({ "start": ms(w.start), "deployments": w.deployments, "change_failures": w.change_failures, "lead_time_s": w.lead_time_s, "recovery_s": w.recovery_s })).collect();
    let rates: Vec<Value> = levels::FAILURE_RATES.iter().map(|(l, r)| json!({ "level": l, "rate": r })).collect();
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "from": ms(s.from),
            "to": ms(s.to),
            "deployments": s.deployments,
            "per_day": s.per_day,
            "frequency_level": levels::frequency(s.per_day).filter(|_| s.deployments > 0),
            "lead_time_s": s.lead_time_s,
            "lead_time_level": s.lead_time_s.and_then(levels::lead_time),
            "lead_deployments": s.lead_deployments,
            "change_failures": s.change_failures,
            "failure_rate": s.failure_rate,
            "failure_rates": rates,
            "recovery_s": s.recovery_s,
            "recovery_level": s.recovery_s.and_then(levels::recovery),
            "recovered": s.recovered,
            "failures": s.failures,
            "open_failures": s.open_failures,
            "restore_s": s.restore_s,
            "weeks": weeks,
            "services": services,
            "envs": envs,
            "teams": teams,
        }),
    ))
}

/// `GET dora/summary`: the four keys (kiln's routes are in `crate::observe::api::route`).
pub fn summary_of(q: &HashMap<String, String>) -> Result<Response, Fail> {
    summary(dora()?, q)
}

/// `POST dora/deployments`: a list of deployments.
pub async fn deployments(req: Request) -> Result<Response, Fail> {
    let l: ListIn = unwrap(read_json(req.into_body(), BODY).await?)?;
    list_deployments(dora()?, l)
}

/// `POST dora/failures`: a list of failures.
pub async fn failures(req: Request) -> Result<Response, Fail> {
    let l: ListIn = unwrap(read_json(req.into_body(), BODY).await?)?;
    list_failures(dora()?, l)
}

/// `GET dora/deployments/{id}`.
pub fn deployment(id: &str) -> Result<Response, Fail> {
    dora()?.read(|l| l.deployment(id).map(deployment_json)).map(|d| srv::json(StatusCode::OK, &json!({ "data": d }))).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such deployment"))
}

/// `GET dora/failures/{id}`.
pub fn failure(id: &str) -> Result<Response, Fail> {
    dora()?.read(|l| l.failure(id).map(failure_json)).map(|f| srv::json(StatusCode::OK, &json!({ "data": f }))).ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such failure"))
}

/// `DELETE dora/deployments/{id}`.
pub async fn remove_deployment(id: &str) -> Result<Response, Fail> {
    remove(dora().map(|d| d.remove_deployment(id)), "deployment").await
}

/// `DELETE dora/failures/{id}`.
pub async fn remove_failure(id: &str) -> Result<Response, Fail> {
    remove(dora().map(|d| d.remove_failure(id)), "failure").await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000 * 1_000_000_000;

    /// Seconds, milliseconds and nanoseconds told apart by size; microseconds and times before 2001
    /// refused, saying why.
    #[test]
    fn times_are_read_in_seconds_milliseconds_or_nanoseconds() {
        let ns = 1_790_000_000_123_000_000_i64;
        assert_eq!(time_ns(&json!(1_790_000_000), "t"), Ok(1_790_000_000_000_000_000));
        assert_eq!(time_ns(&json!(1_790_000_000_123_i64), "t"), Ok(ns));
        assert_eq!(time_ns(&json!(ns), "t"), Ok(ns));
        assert_eq!(time_ns(&json!(1_790_000_000.0), "t"), Ok(1_790_000_000_000_000_000));
        for bad in [json!(1_790_000_000_123_000_i64), json!(5), json!("1790000000"), json!(1.5e9 + 0.5)] {
            assert!(time_ns(&bad, "t").is_err(), "{bad}");
        }
    }

    /// Datadog's body checked: its required fields, an id's form, its tags, its commit; a deployment
    /// that ends before it starts, or in the future, refused.
    #[test]
    fn deployments_are_checked_as_datadog_checks_them() {
        let body = |v: Value| -> DeploymentIn { serde_json::from_value(v).unwrap() };
        let ok = deployment_of(
            body(json!({ "service": " shop ", "env": "prod", "started_at": 1_789_999_000, "finished_at": 1_790_000_000_000_i64, "git": { "commit_sha": "ABCDEF1234567", "repository_url": "https://github.com/acme/shop" }, "custom_tags": ["region:eu"], "version": "v1" })),
            "api",
            NOW,
        )
        .unwrap();
        assert_eq!((ok.service.as_str(), ok.env.as_str(), ok.started_at, ok.finished_at), ("shop", "prod", 1_789_999_000_000_000_000, 1_790_000_000_000_000_000));
        assert_eq!(ok.git.as_ref().map(|g| g.commit_sha.as_str()), Some("abcdef1234567"));
        assert_eq!(ok.id.len(), 32);
        for (v, says) in [
            (json!({ "service": "", "started_at": 1_789_999_000, "finished_at": 1_790_000_000 }), "service"),
            (json!({ "service": "s", "started_at": 1_790_000_001, "finished_at": 1_790_000_000 }), "after finished_at"),
            (json!({ "service": "s", "started_at": 1_790_000_000, "finished_at": 1_790_009_000 }), "future"),
            (json!({ "service": "s", "id": "short", "started_at": 1_789_999_000, "finished_at": 1_790_000_000 }), "16 to 128"),
            (json!({ "service": "s", "id": "has a space in it, too", "started_at": 1_789_999_000, "finished_at": 1_790_000_000 }), "16 to 128"),
            (json!({ "service": "s", "custom_tags": ["nokey"], "started_at": 1_789_999_000, "finished_at": 1_790_000_000 }), "key:value"),
            (json!({ "service": "s", "git": { "commit_sha": "main", "repository_url": "x" }, "started_at": 1_789_999_000, "finished_at": 1_790_000_000 }), "sha"),
        ] {
            let e = deployment_of(body(v.clone()), "api", NOW).unwrap_err();
            assert!(e.contains(says), "{v}: {e}");
        }
    }

    #[test]
    fn failures_need_a_service_or_a_team() {
        let body = |v: Value| -> FailureIn { serde_json::from_value(v).unwrap() };
        assert!(failure_of(body(json!({ "started_at": 1_789_999_000 })), NOW).unwrap_err().contains("services or team"));
        let f = failure_of(body(json!({ "team": "core", "started_at": 1_789_999_000, "finished_at": null })), NOW).unwrap();
        assert_eq!((f.team.as_str(), f.finished_at), ("core", None));
        assert!(failure_of(body(json!({ "services": ["a"], "started_at": 1_789_999_000, "finished_at": 1_789_998_000 })), NOW).unwrap_err().contains("before"));
    }
}
