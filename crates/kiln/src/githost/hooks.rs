//! GitLab's and Bitbucket's webhooks (docs/ci.md W23-R04, R05; docs/deploy-api.md §7): checked
//! with the host's webhook secret (GitLab's `X-Gitlab-Token`, Bitbucket's `X-Hub-Signature`), then
//! a push hints that a poll is due, and a pipeline, job, deployment or build status brings the run
//! it's about into kiln at once.

use std::sync::Arc;

use http::{StatusCode, header};
use serde_json::Value;

use super::{bitbucket, gitlab};
use crate::ci::Fail;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// The kiln repository a delivery's project is, by its path on the host.
async fn repo_named(hub: &Hub, path: &str, name_of: impl Fn(&Repo) -> Option<String>) -> Option<Repo> {
    hub.store.repos().await.ok()?.into_iter().find(|r| name_of(r).is_some_and(|n| n.eq_ignore_ascii_case(path)))
}

/// `POST /api/hooks/gitlab`: a GitLab's events, with the token its webhook was given.
pub async fn gitlab(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let head = |n: &'static str| srv::header_str(&req, &header::HeaderName::from_static(n)).unwrap_or("").to_owned();
    let (event, token) = (head("x-gitlab-event"), head("x-gitlab-token"));
    let body = srv::body_bytes(req.into_body(), 25 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let v: Value = serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("not JSON: {e}")))?;
    // Which GitLab sent it: the one the project's page is on.
    let page = v.pointer("/project/web_url").or_else(|| v.pointer("/repository/homepage")).and_then(Value::as_str).unwrap_or_default();
    let hosts = gitlab::hosts(hub).await;
    let Some(host) = hosts.into_iter().find(|g| page.starts_with(&format!("{}/", g.web))) else { return Err((StatusCode::NOT_FOUND, "no GitLab here has that project".into())) };
    let Some(secret) = host.webhook_secret.clone() else { return Err((StatusCode::SERVICE_UNAVAILABLE, format!("{} has no webhook secret here", host.web))) };
    if !crate::util::ct_eq(secret.as_bytes(), token.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "bad token".into()));
    }
    let path = v.pointer("/project/path_with_namespace").and_then(Value::as_str).unwrap_or_default().to_owned();
    let Some(repo) = repo_named(hub, &path, |r| host.name_of(r)).await else {
        hub.gitops_kick.notify();
        return Ok(srv::status(StatusCode::ACCEPTED));
    };
    match event.as_str() {
        "Push Hook" | "Tag Push Hook" => hub.gitops_kick.notify(),
        "Pipeline Hook" | "Job Hook" | "Deployment Hook" => {
            let hub2 = Arc::clone(hub);
            crate::rt::spawn(async move {
                if let Err(e) = crate::ci::providers::gitlab_ci::on_event(&hub2, &repo, &event, &v).await {
                    tracing::debug!(event = %event, error = %format!("{e:#}"), "a GitLab pipeline didn't sync");
                }
            });
        }
        // Merge requests and their notes: the review pages read them from GitLab as they're shown.
        _ => {}
    }
    Ok(srv::status(StatusCode::ACCEPTED))
}

/// `POST /api/hooks/bitbucket`: Bitbucket's events, signed with the webhook's secret.
pub async fn bitbucket(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(host) = bitbucket::host(hub).await else { return Err((StatusCode::NOT_FOUND, "no Bitbucket here".into())) };
    let Some(secret) = host.webhook_secret.clone() else { return Err((StatusCode::SERVICE_UNAVAILABLE, "Bitbucket has no webhook secret here".into())) };
    let head = |n: &'static str| srv::header_str(&req, &header::HeaderName::from_static(n)).unwrap_or("").to_owned();
    let (event, sig) = (head("x-event-key"), head("x-hub-signature"));
    let body = srv::body_bytes(req.into_body(), 25 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let want = format!("sha256={}", crate::util::hmac_hex(secret.as_bytes(), &body));
    if !crate::util::ct_eq(want.as_bytes(), sig.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "bad signature".into()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| (StatusCode::BAD_REQUEST, format!("not JSON: {e}")))?;
    let full = v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default().to_owned();
    let Some(repo) = repo_named(hub, &full, |r| host.name_of(r)).await else {
        hub.gitops_kick.notify();
        return Ok(srv::status(StatusCode::ACCEPTED));
    };
    match event.as_str() {
        "repo:push" => hub.gitops_kick.notify(),
        "repo:commit_status_created" | "repo:commit_status_updated" => {
            let hub2 = Arc::clone(hub);
            crate::rt::spawn(async move {
                if let Err(e) = crate::ci::providers::bitbucket_pipelines::on_event(&hub2, &repo, &event).await {
                    tracing::debug!(event = %event, error = %format!("{e:#}"), "Bitbucket's pipelines didn't sync");
                }
            });
        }
        _ => {}
    }
    Ok(srv::status(StatusCode::ACCEPTED))
}
