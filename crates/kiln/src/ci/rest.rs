//! The facade's REST half (docs/ci.md W14-R11): GitHub's REST API, as far as kiln implements it,
//! at `/api/v3` (GitHub Enterprise Server's path), for a leg's `GITHUB_TOKEN`. GraphQL is at
//! `/api/graphql`.
//!
//! - Implemented so far:
//!   - the artifact endpoints that `download-artifact` uses across runs (W08-R08);
//!   - `workflow_dispatch` and `repository_dispatch` (W03-R11).
//! - Anything else answers 501 with `{"message": "not implemented by kiln: …"}`, and the leg gets
//!   a warning naming the route, so gaps are visible and get filled.
//!
//! [`web`] sends GitHub-shaped page links (`/<owner>/<repo>/actions/runs/<id>…`, which actions
//! print, such as `upload-artifact`'s `artifact-url`) to kiln's own pages.

use std::sync::Arc;

use http::{Method, StatusCode, header};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::services::blob::{self, Target};
use super::token::Claims;
use super::{Fail, runs, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

const DOCS: &str = "https://github.com/hyper-light/kiln/blob/main/docs/ci.md#w14-code-hosting-stacked-diffs-review-and-landing";

fn message(status: StatusCode, msg: &str) -> Response {
    srv::json(status, &json!({ "message": msg, "documentation_url": DOCS }))
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// `/api/v3/<rest>`.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Ok(message(StatusCode::UNAUTHORIZED, "Bad credentials")) };
    let method = req.method().clone();
    let q = srv::query(&req);
    let origin = blob::origin(&req);
    match (&method, rest) {
        (&Method::GET, ["repos", owner, repo, "actions", "runs", run, "artifacts"]) => {
            let Some(r) = repo_run(hub, &claims, owner, repo, run).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            artifacts(hub, &origin, Scope::Run(r), q.get("name").cloned(), &q).await
        }
        (&Method::GET, ["repos", owner, repo, "actions", "artifacts"]) => {
            let Some(repo_id) = repo_of(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            artifacts(hub, &origin, Scope::Repo(repo_id), q.get("name").cloned(), &q).await
        }
        (&Method::GET, ["repos", owner, repo, "actions", "artifacts", id]) => {
            let Some(repo_id) = repo_of(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            match artifact(hub, &repo_id, id).await? {
                Some(a) => Ok(srv::json(StatusCode::OK, &a.json(&origin, owner, repo))),
                None => Ok(message(StatusCode::NOT_FOUND, "Not Found")),
            }
        }
        (&Method::GET, ["repos", owner, repo, "actions", "artifacts", id, "zip"]) => {
            let Some(repo_id) = repo_of(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            match artifact(hub, &repo_id, id).await? {
                Some(a) if !a.expired => {
                    // GitHub answers with a redirect to a short-lived download URL.
                    let url = blob::url(hub, &origin, Target::Artifact(a.id), "r", 60);
                    Ok(srv::with_header(srv::status(StatusCode::FOUND), header::LOCATION, &url))
                }
                Some(_) => Ok(message(StatusCode::GONE, "Artifact has expired")),
                None => Ok(message(StatusCode::NOT_FOUND, "Not Found")),
            }
        }
        (&Method::DELETE, ["repos", owner, repo, "actions", "artifacts", id]) => {
            let Some(repo_id) = repo_of(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            let Some(a) = artifact(hub, &repo_id, id).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            let aid = a.id;
            hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_artifacts WHERE id = ?1", [aid])?)).await.map_err(internal)?;
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        // W03-R11: dispatches by a job's token (GitHub lets those start runs, unlike other
        // events a token causes). Only trusted runs may.
        (&Method::POST, ["repos", owner, repo, "actions", "workflows", which, "dispatches"]) => {
            let Some(r) = trusted_repo(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            let ask: Value = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let git_ref = ask.get("ref").and_then(Value::as_str).unwrap_or_default().to_owned();
            let inputs = ask.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default();
            match super::events::workflow_dispatch(hub, &r, &git_ref, which, &inputs, "github-actions[bot]").await {
                Ok(()) => Ok(srv::status(StatusCode::NO_CONTENT)),
                Err(e) => Ok(message(e.status, &e.message)),
            }
        }
        (&Method::POST, ["repos", owner, repo, "dispatches"]) => {
            let Some(r) = trusted_repo(hub, &claims, owner, repo).await? else { return Ok(message(StatusCode::NOT_FOUND, "Not Found")) };
            let ask: Value = srv::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let event_type = ask.get("event_type").and_then(Value::as_str).unwrap_or_default().to_owned();
            let payload = ask.get("client_payload").cloned().unwrap_or(Value::Null);
            match super::events::repository_dispatch(hub, &r, &event_type, &payload, "github-actions[bot]").await {
                Ok(_) => Ok(srv::status(StatusCode::NO_CONTENT)),
                Err(e) => Ok(message(e.status, &e.message)),
            }
        }
        _ => not_implemented(hub, &claims, &method, &format!("/api/v3/{}", rest.join("/"))).await,
    }
}

/// The token's repository when its run is trusted (W03-R08): untrusted runs can't start others.
async fn trusted_repo(hub: &Hub, claims: &Claims, owner: &str, repo: &str) -> Result<Option<crate::store::Repo>, Fail> {
    if repo_of(hub, claims, owner, repo).await?.is_none() {
        return Ok(None);
    }
    let run = claims.run.clone();
    let trusted: bool = hub.store.call(move |c| Ok(c.query_row("SELECT trust = 'trusted' FROM runs WHERE id = ?1", [&run], |r| r.get(0)).optional()?.unwrap_or(false))).await.map_err(internal)?;
    if !trusted {
        return Ok(None);
    }
    let repos = hub.store.repos().await.map_err(internal)?;
    Ok(repos.into_iter().find(|r| r.id == claims.repo))
}

/// `/api/graphql`.
pub async fn graphql(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else { return Ok(message(StatusCode::UNAUTHORIZED, "Bad credentials")) };
    not_implemented(hub, &claims, req.method(), "/api/graphql").await
}

/// 501, and a warning on the leg naming the route.
async fn not_implemented(hub: &Hub, claims: &Claims, method: &Method, path: &str) -> Result<Response, Fail> {
    let what = format!("not implemented by kiln: {method} {path}");
    tracing::info!(leg = %claims.sub, route = %path, "facade route not implemented");
    let _ = runs::annotation(hub, &claims.sub, None, "warning", Some("GitHub API".into()), &format!("This job called GitHub's API at a route kiln doesn't implement yet ({method} {path}); it got 501."), None, None, None, None, None).await;
    Ok(message(StatusCode::NOT_IMPLEMENTED, &what))
}

/// The run (`runs.id`) a `run_id` names, when it is the token's repository's.
async fn repo_run(hub: &Hub, claims: &Claims, owner: &str, repo: &str, run: &str) -> Result<Option<String>, Fail> {
    let Some(repo_id) = repo_of(hub, claims, owner, repo).await? else { return Ok(None) };
    let Ok(rowid) = run.parse::<i64>() else { return Ok(None) };
    hub.store.call(move |c| Ok(c.query_row("SELECT id FROM runs WHERE rowid = ?1 AND repo_id = ?2", params![rowid, repo_id], |r| r.get::<_, String>(0)).optional()?)).await.map_err(internal)
}

/// The repository's id, when it is the one the token's run belongs to: a job's token reads its
/// own repository only.
async fn repo_of(hub: &Hub, claims: &Claims, owner: &str, repo: &str) -> Result<Option<String>, Fail> {
    let full = format!("{owner}/{repo}");
    let repos = hub.store.repos().await.map_err(internal)?;
    Ok(repos.into_iter().find(|r| r.id == claims.repo && r.name.eq_ignore_ascii_case(&full)).map(|r| r.id))
}

enum Scope {
    Run(String),
    Repo(String),
}

struct Artifact {
    id: i64,
    name: String,
    size: i64,
    digest: Option<String>,
    created_at: i64,
    expires_at: Option<i64>,
    expired: bool,
    run_rowid: i64,
    branch: String,
    sha: String,
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default()
}

impl Artifact {
    /// GitHub's artifact object.
    fn json(&self, origin: &str, owner: &str, repo: &str) -> Value {
        let api = format!("{origin}/api/v3/repos/{owner}/{repo}/actions/artifacts/{}", self.id);
        json!({
            "id": self.id,
            "node_id": format!("kiln-artifact-{}", self.id),
            "name": self.name,
            "size_in_bytes": self.size,
            "url": api,
            "archive_download_url": format!("{api}/zip"),
            "expired": self.expired,
            "created_at": rfc3339(self.created_at),
            "updated_at": rfc3339(self.created_at),
            "expires_at": self.expires_at.map(rfc3339),
            "digest": self.digest,
            "workflow_run": { "id": self.run_rowid, "head_branch": self.branch, "head_sha": self.sha },
        })
    }
}

const SELECT: &str = "SELECT a.id, a.name, COALESCE(a.size, 0), a.digest, a.created_at, a.expires_at, a.object IS NULL OR (a.expires_at IS NOT NULL AND a.expires_at <= ?1), r.rowid, r.branch, r.sha
     FROM ci_artifacts a JOIN runs r ON r.id = a.run_id WHERE a.finalized = 1";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact { id: r.get(0)?, name: r.get(1)?, size: r.get(2)?, digest: r.get(3)?, created_at: r.get(4)?, expires_at: r.get(5)?, expired: r.get(6)?, run_rowid: r.get(7)?, branch: r.get(8)?, sha: r.get(9)? })
}

async fn artifact(hub: &Hub, repo_id: &str, id: &str) -> Result<Option<Artifact>, Fail> {
    let Ok(id) = id.parse::<i64>() else { return Ok(None) };
    let repo_id = repo_id.to_owned();
    hub.store
        .call(move |c| Ok(c.query_row(&format!("{SELECT} AND a.id = ?2 AND r.repo_id = ?3"), params![crate::util::now_unix(), id, repo_id], row).optional()?))
        .await
        .map_err(internal)
}

async fn artifacts(hub: &Hub, origin: &str, scope: Scope, name: Option<String>, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let per_page = q.get("per_page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(30).clamp(1, 100);
    let page = q.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let (owner_repo, list): (Option<String>, Vec<Artifact>) = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            let (filter, key) = match &scope {
                Scope::Run(run) => ("AND a.run_id = ?2", run.clone()),
                Scope::Repo(repo) => ("AND r.repo_id = ?2", repo.clone()),
            };
            let mut q = c.prepare(&format!("{SELECT} {filter} AND (?3 IS NULL OR a.name = ?3) ORDER BY a.id DESC"))?;
            let list = q.query_map(params![now, key, name], row)?.collect::<Result<Vec<_>, _>>()?;
            let repo_name: Option<String> = match &scope {
                Scope::Run(run) => c.query_row("SELECT p.name FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1", [run], |r| r.get(0)).optional()?,
                Scope::Repo(repo) => c.query_row("SELECT name FROM repos WHERE id = ?1", [repo], |r| r.get(0)).optional()?,
            };
            Ok((repo_name, list))
        })
        .await
        .map_err(internal)?;
    let full = owner_repo.unwrap_or_default();
    let (owner, repo) = full.split_once('/').unwrap_or((&full, ""));
    let total = list.len();
    let start = usize::try_from((page - 1) * per_page).unwrap_or(usize::MAX);
    let shown: Vec<Value> = list.iter().skip(start).take(usize::try_from(per_page).unwrap_or(30)).map(|a| a.json(origin, owner, repo)).collect();
    Ok(srv::json(StatusCode::OK, &json!({ "total_count": total, "artifacts": shown })))
}

/// GitHub-shaped page links: `/<owner>/<repo>/actions/runs/<run_id>[/…]` to the run's page.
pub fn is_web(seg: &[&str]) -> bool {
    matches!(seg, [owner, _, "actions", "runs", run, ..] if !super::api::reserved(owner) && run.parse::<i64>().is_ok())
}

pub async fn web(hub: &Arc<Hub>, seg: &[&str]) -> Result<Response, Fail> {
    let [owner, repo, "actions", "runs", run, ..] = seg else { return Err((StatusCode::NOT_FOUND, "not found".into())) };
    let full = format!("{owner}/{repo}");
    let rowid: i64 = run.parse().unwrap_or(-1);
    let id: Option<String> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT r.id FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.rowid = ?1 AND p.name = ?2 COLLATE NOCASE", params![rowid, full], |r| r.get(0)).optional()?))
        .await
        .map_err(internal)?;
    match id {
        Some(id) => Ok(srv::with_header(srv::status(StatusCode::FOUND), header::LOCATION, &format!("/runs/{id}"))),
        None => Err((StatusCode::NOT_FOUND, "no such run".into())),
    }
}
