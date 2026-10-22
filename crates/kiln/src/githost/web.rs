//! The GitHub App's web half (docs/ci.md W14-R10): its webhook, and GitHub's App manifest flow,
//! which makes the App in one step (kiln fills in its name, permissions, events and URLs; the person
//! clicks Create on GitHub; GitHub hands the App's id, key and secrets back to kiln).

use std::sync::Arc;

use http::{StatusCode, header};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::github;
use crate::ci::Fail;
use crate::http::client::Method;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// `POST /api/hooks/github/app`: the App's events, signed with its webhook secret. GitHub's
/// "Re-run" on a check run reruns kiln's run; the rest reach the workflows as GitHub's events do.
pub async fn webhook(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(app) = github::app(hub).await else { return Err((StatusCode::NOT_FOUND, "no GitHub App here".into())) };
    let Some(secret) = app.webhook_secret.clone() else { return Err((StatusCode::SERVICE_UNAVAILABLE, "the GitHub App has no webhook secret here".into())) };
    let head = |n: &'static str| srv::header_str(&req, &header::HeaderName::from_static(n)).unwrap_or("").to_owned();
    let (sig, event, delivery) = (head("x-hub-signature-256"), head("x-github-event"), head("x-github-delivery"));
    let body = srv::body_bytes(req.into_body(), 25 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let want = format!("sha256={}", crate::util::hmac_hex(secret.as_bytes(), &body));
    if !crate::util::ct_eq(want.as_bytes(), sig.as_bytes()) {
        return Err((StatusCode::UNAUTHORIZED, "bad signature".into()));
    }
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let action = v.get("action").and_then(Value::as_str).unwrap_or_default();
    match (event.as_str(), action) {
        ("check_run", "rerequested") => {
            let run = v.pointer("/check_run/external_id").and_then(Value::as_str).unwrap_or_default().to_owned();
            if !run.is_empty() {
                let r = crate::ci::runs_api::rerun(hub, &run, &json!({ "mode": "all" })).await;
                hub.store.audit("github".into(), "ci.runs.rerun".into(), format!("{run} from GitHub's Re-run ({})", if r.is_ok() { "started" } else { "refused" })).await;
            }
        }
        ("check_suite", "rerequested") => {
            let (sha, full) = (v.pointer("/check_suite/head_sha").and_then(Value::as_str).unwrap_or_default().to_owned(), v.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default().to_owned());
            for run in runs_of(hub, &full, &sha).await {
                let _ = crate::ci::runs_api::rerun(hub, &run, &json!({ "mode": "failed" })).await;
            }
        }
        // GitHub Actions' runs, for repositories whose CI provider it is (W23-R03).
        ("workflow_run" | "workflow_job" | "deployment_review", _) => {
            let (hub2, event2) = (Arc::clone(hub), event.clone());
            crate::rt::spawn(async move {
                if let Err(e) = crate::ci::providers::github_actions::on_event(&hub2, &event2, &v).await {
                    tracing::debug!(event = %event2, error = %format!("{e:#}"), "a GitHub Actions run didn't sync");
                }
            });
        }
        ("installation" | "installation_repositories", _) => {
            let account = v.pointer("/installation/account/login").and_then(Value::as_str).unwrap_or_default();
            hub.store.audit("github".into(), format!("github.{event}"), format!("{action} for {account}")).await;
        }
        _ => {
            // A release repository's push, or a release's pull request (docs/deploy-api.md §7).
            if matches!(event.as_str(), "push" | "pull_request")
                && let Some(d) = crate::deploy::get()
            {
                crate::deploy::remote::on_github_event(&d, &event, &v).await;
            }
            let hub2 = Arc::clone(hub);
            let delivery = (!delivery.is_empty()).then_some(delivery);
            crate::rt::spawn(async move {
                if let Err(e) = crate::ci::hooks::github(&hub2, &event, delivery, &body).await {
                    tracing::warn!(event = %event, error = %format!("{e:#}"), "the GitHub App's event");
                    hub2.gitops_kick.notify();
                }
            });
        }
    }
    Ok(srv::status(StatusCode::ACCEPTED))
}

/// KilnCI's runs of GitHub's repository `full` at `sha`.
async fn runs_of(hub: &Hub, full: &str, sha: &str) -> Vec<String> {
    let Some(app) = github::app(hub).await else { return Vec::new() };
    let repos = hub.store.repos().await.unwrap_or_default();
    let Some(repo) = repos.into_iter().find(|r| app.name_of(r).is_some_and(|n| n.eq_ignore_ascii_case(full))) else { return Vec::new() };
    let (id, sha) = (repo.id.clone(), sha.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT id FROM runs WHERE repo_id = ?1 AND sha = ?2 AND workflow_id IS NOT NULL")?;
            let rows = st.query_map(params![id, sha], |r| r.get::<_, String>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .unwrap_or_default()
}

/// The App kiln would make: its manifest (GitHub's App manifest format).
fn manifest(hub: &Hub) -> Value {
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let host = super::host_of(&base);
    // GitHub delivers webhooks only where it reaches: a kiln on a private address polls instead.
    let name = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h).trim_matches(['[', ']']);
    let private = name == "localhost" || name.parse::<std::net::IpAddr>().is_ok_and(crate::discovery::is_private);
    json!({
        "name": format!("kiln ({host})"),
        "url": base,
        "hook_attributes": { "url": format!("{base}/api/hooks/github/app"), "active": !private },
        "redirect_url": format!("{base}/api/ci/github/app/callback"),
        "callback_urls": [format!("{base}/api/auth/github/callback")],
        "public": false,
        // `administration`: release repositories made in an organization (docs/deploy-api.md §7).
        "default_permissions": {
            "checks": "write", "statuses": "write", "contents": "write", "pull_requests": "write", "issues": "write",
            "actions": "write", "deployments": "write", "metadata": "read", "members": "read", "workflows": "write",
            "administration": "write",
        },
        "default_events": ["push", "pull_request", "pull_request_review", "pull_request_review_comment", "issue_comment", "check_run", "check_suite", "create", "delete", "release", "workflow_run", "workflow_job"],
    })
}

/// `POST /api/ci/github/app/manifest` `{ org? }`: where to send the manifest and what it is. The
/// page posts it as a form (`manifest`) to `action`, GitHub shows it, the person creates the App.
pub async fn start(hub: &Hub, caller: &str, body: &Value) -> Result<Response, Fail> {
    let state = crate::util::random_token();
    let (st, by) = (state.clone(), caller.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute("DELETE FROM github_manifests WHERE created_at < ?1", [now - 3600])?;
            c.execute("INSERT INTO github_manifests (state, created_by, created_at) VALUES (?1, ?2, ?3)", params![st, by, now])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    let web = hub.cfg.ci.github.web_url.trim_end_matches('/').to_owned();
    let action = match body.get("org").and_then(Value::as_str).filter(|o| !o.trim().is_empty()) {
        Some(org) => format!("{web}/organizations/{}/settings/apps/new?state={state}", crate::ci::cli::enc(org.trim())),
        None => format!("{web}/settings/apps/new?state={state}"),
    };
    Ok(srv::json(StatusCode::OK, &json!({ "action": action, "manifest": manifest(hub).to_string() })))
}

/// `GET /api/ci/github/app/callback?code&state`: GitHub, back with the App it made. Its id, key and
/// secrets are kept (the key and secrets sealed); the browser goes back to Settings.
pub async fn callback(hub: &Arc<Hub>, req: &Request) -> Result<Response, Fail> {
    let q = srv::query(req);
    let (code, state) = (q.get("code").cloned().unwrap_or_default(), q.get("state").cloned().unwrap_or_default());
    let st = state.clone();
    let asked: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT created_by FROM github_manifests WHERE state = ?1 AND created_at > ?2", params![st, crate::util::now_unix() - 3600], |r| r.get(0)).optional()?)).await.map_err(fail)?;
    let Some(by) = asked else { return Err((StatusCode::BAD_REQUEST, "this App creation wasn't asked for here, or took over an hour".into())) };
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err((StatusCode::BAD_REQUEST, "GitHub sent no code".into()));
    }
    // Asking GitHub needs no App yet: the code is the credential.
    let api = hub.cfg.ci.github.api_url.trim_end_matches('/').to_owned();
    let client = crate::http::client::github().map_err(fail)?;
    let resp = client.request(Method::Post, &format!("{api}/app-manifests/{code}/conversions")).header("Accept", "application/vnd.github+json").header("User-Agent", "kiln").send().await.map_err(fail)?;
    let status = resp.status();
    let made: Value = resp.json(1 << 20).await.map_err(fail)?;
    if status != 201 {
        return Err((StatusCode::BAD_GATEWAY, format!("GitHub didn't hand the App over ({status}): {}", github::said(&made))));
    }
    let id = github::keep(hub, &made).await.map_err(fail)?;
    let st = state.clone();
    hub.store.call(move |c| Ok(c.execute("DELETE FROM github_manifests WHERE state = ?1", [&st]).map(|_| ())?)).await.map_err(fail)?;
    hub.store.audit(by, "github.app.create".into(), format!("App {id} ({})", made.get("slug").and_then(Value::as_str).unwrap_or_default())).await;
    let base = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    Ok(srv::with_header(srv::status(StatusCode::SEE_OTHER), header::LOCATION, &format!("{base}/settings?github=created")))
}

/// `GET /api/ci/github/app`: the App kiln acts through, if any, and where to install it.
pub async fn show(hub: &Hub) -> Result<Response, Fail> {
    let from_config = hub.cfg.ci.github.app_id.is_some() && hub.cfg.ci.github.private_key_file.is_some();
    let Some(app) = github::app(hub).await else {
        return Ok(srv::json(StatusCode::OK, &json!({ "configured": false, "web_url": hub.cfg.ci.github.web_url })));
    };
    let install = app.html_url.as_deref().map(|u| format!("{u}/installations/new"));
    Ok(srv::json(
        StatusCode::OK,
        &json!({ "configured": true, "source": if from_config { "config" } else { "manifest" }, "id": app.id, "slug": app.slug, "html_url": app.html_url, "install_url": install, "webhook": app.webhook_secret.is_some(), "web_url": app.web, "api_url": app.api }),
    ))
}
