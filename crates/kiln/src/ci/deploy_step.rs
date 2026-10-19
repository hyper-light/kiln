//! `kiln/deploy@v1`'s server half (docs/ci.md W15-R05): CI drives CD. A leg bound to an
//! environment (`environment:` on its job: the environment's protection passed before it started,
//! W15-R04) releases an app there through the deploy service, whose own gates apply too
//! (approvals, windows, soak), and follows the release to its end.
//!
//! - `POST /_kiln/deploy/releases` `{ app?, environment, version?, artifacts?, attempt }`: the
//!   release, made once per attempt (a retry of the request gets the same one). Without `app`,
//!   the one app the leg's repository is the source of and registers in the environment.
//! - `GET /_kiln/deploy/releases/<id>`: one the leg made, with what its provider says of it
//!   (`sync`, `health`, the operation's `phase`, and whether it `landed`: synced and healthy).

use std::collections::BTreeMap;
use std::sync::Arc;

use http::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

fn msg(status: StatusCode, message: &str) -> Response {
    srv::json(status, &json!({ "message": message }))
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Who a leg's releases are asked by.
fn who(leg: &str) -> String {
    format!("job:{leg}")
}

/// `/_kiln/deploy/<rest>`, for a leg's token.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else {
        return Ok(msg(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed"));
    };
    let method = req.method().clone();
    match (method, rest) {
        (Method::POST, ["releases"]) => create(hub, &claims.sub, &claims.repo, req).await,
        (Method::GET, ["releases", id]) => get(&claims.sub, id).await,
        _ => Ok(msg(StatusCode::NOT_FOUND, "not found")),
    }
}

#[derive(Deserialize)]
struct Ask {
    #[serde(default)]
    app: Option<String>,
    environment: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    artifacts: BTreeMap<String, crate::deploy::model::Artifact>,
    #[serde(default)]
    attempt: u32,
}

/// The one app `repo_id`'s repository is the source of that `environment` registers; else why not.
async fn default_app(hub: &Hub, d: &crate::deploy::Deploy, repo_id: &str, environment: &str) -> Result<Result<String, String>, Fail> {
    let Some(repo) = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.id == repo_id) else { return Ok(Err("which app? name it (`app:`)".into())) };
    let registered: Vec<String> = crate::deploy::store::registrations(&d.hub.store, None, Some(environment)).await.map_err(internal)?.into_iter().map(|r| r.app).collect();
    let mut apps: Vec<String> = crate::deploy::store::apps(&d.hub.store)
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|a| !a.archived && a.source.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&repo.name)) && registered.contains(&a.name))
        .map(|a| a.name)
        .collect();
    Ok(match apps.len() {
        1 => Ok(apps.remove(0)),
        0 => Err(format!("which app? no app from {} is registered in {environment}: name one (`app:`)", repo.name)),
        _ => Err(format!("which app? {} are from {} and registered in {environment}: name one (`app:`)", apps.join(", "), repo.name)),
    })
}

/// The release, when the leg's job is bound to the environment.
async fn create(hub: &Arc<Hub>, leg: &str, repo_id: &str, req: Request) -> Result<Response, Fail> {
    let ask: Ask = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let l = leg.to_owned();
    let bound: Vec<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT DISTINCT environment FROM ci_deployments WHERE leg_id = ?1")?;
            let rows = q.query_map([&l], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)?;
    if !bound.iter().any(|e| e.eq_ignore_ascii_case(&ask.environment)) {
        let has = if bound.is_empty() { "none".to_owned() } else { bound.join(", ") };
        return Ok(msg(
            StatusCode::FORBIDDEN,
            &format!("kiln/deploy releases to the job's own environment (`environment: {}` on the job, its protection passed before it started); this job's: {has}", ask.environment),
        ));
    }
    let Some(d) = crate::deploy::get() else { return Ok(msg(StatusCode::SERVICE_UNAVAILABLE, "the deploy service isn't running on this server")) };
    let app = match ask.app.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => a.to_owned(),
        None => match default_app(hub, &d, repo_id, &ask.environment).await? {
            Ok(a) => a,
            Err(why) => return Ok(msg(StatusCode::BAD_REQUEST, &why)),
        },
    };
    let who = crate::deploy::flow::Who { name: who(leg), agent: false };
    let flow_ask = crate::deploy::flow::Ask {
        app: app.clone(),
        environment: ask.environment.clone(),
        version: ask.version.clone(),
        artifacts: ask.artifacts.clone(),
        values: None,
        rollback: None,
        enabled: None,
        promote_from: None,
        keep: false,
        idempotency_key: Some(format!("ci:{leg}:{}:{app}:{}", ask.attempt, ask.environment)),
    };
    match crate::deploy::flow::release(&d, &who, flow_ask).await {
        Ok(r) => Ok(srv::json(StatusCode::CREATED, &release_json(&r))),
        Err((status, why)) => Ok(msg(status, &why)),
    }
}

fn release_json(r: &crate::deploy::model::Release) -> Value {
    let said = |k: &str| r.provider.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "id": r.id, "app": r.app, "environment": r.environment, "state": r.state.as_str(), "version": r.version, "message": r.message,
        "landed": r.provider.get("landed_at").is_some_and(|v| !v.is_null()), "sync": said("sync"), "health": said("health"),
        "phase": r.provider.pointer("/operation/phase").cloned().unwrap_or(Value::Null),
    })
}

/// One release this leg made.
async fn get(leg: &str, id: &str) -> Result<Response, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(msg(StatusCode::NOT_FOUND, "no such release")) };
    let Some(d) = crate::deploy::get() else { return Ok(msg(StatusCode::SERVICE_UNAVAILABLE, "the deploy service isn't running on this server")) };
    let found = crate::deploy::store::release(&d.hub.store, id).await.map_err(internal)?;
    match found.filter(|r| r.requested_by == who(leg)) {
        Some(r) => Ok(srv::json(StatusCode::OK, &release_json(&r))),
        None => Ok(msg(StatusCode::NOT_FOUND, "no such release of this job's")),
    }
}
