//! W02's routes (docs/ci.md §4.1): a repository's workflows at a ref, one workflow with its
//! source, model and diagnostics, checking text that isn't committed (the editor's buffer, an
//! agent's draft), and the JSON Schemas the editor completes from.

use std::collections::HashMap;

use http::StatusCode;
use hyper::body::Incoming;
use kiln_ci::expr::Dialect;
use kiln_ci::wf::{self, SchemaKind, Severity};
use serde::Deserialize;
use serde_json::json;

use super::Fail;
use super::load::{self, Loaded};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// A workflow is at most 1 MiB (W02-R03); the rest of the body is JSON's.
const LIMIT: usize = 2 << 20;

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn missing(m: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, m.into())
}

fn internal(e: &anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

#[derive(Deserialize)]
struct Check {
    /// Where the file is or will be: decides what it is (W02-R01). Defaults to a kiln workflow.
    #[serde(default)]
    path: String,
    content: String,
    #[serde(default)]
    dialect: Option<Dialect>,
}

/// `POST /api/ci/validate { path, content, dialect? }` → `{ kind, dialect, diagnostics, outline? }`.
pub async fn validate(body: Incoming) -> Result<Response, Fail> {
    let c: Check = srv::body_json(body, LIMIT).await.map_err(|e| bad(format!("{e:#}")))?;
    let path = if c.path.is_empty() { ".kiln/workflow.yaml".to_owned() } else { c.path };
    let checked = crate::rt::blocking(move || wf::check(&path, &c.content, c.dialect)).await.map_err(|e| internal(&e))?;
    Ok(srv::json(StatusCode::OK, &checked))
}

/// `GET /api/ci/schema/:kind`: `gha`, `kiln`, `overlay` or `action` (W02-R09).
pub fn schema(kind: &str) -> Result<Response, Fail> {
    let k = SchemaKind::parse(kind).ok_or_else(|| missing(format!("no schema `{kind}`: gha, kiln, overlay or action")))?;
    let body = serde_json::to_vec(&wf::schema(k)).map_err(|e| internal(&e.into()))?;
    Ok(srv::response(StatusCode::OK, "application/schema+json", srv::full(body)))
}

async fn find_repo(hub: &Hub, id: &str) -> Result<Repo, Fail> {
    let repos = hub.store.repos().await.map_err(|e| internal(&e))?;
    repos.into_iter().find(|r| r.id == id || r.name == id).ok_or_else(|| missing(format!("no repository `{id}`")))
}

/// The repository, the ref asked for (`HEAD` by default) and the commit it names.
async fn at_ref(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<(Repo, String, String), Fail> {
    let repo = find_repo(hub, repo).await?;
    let r = q.get("ref").filter(|r| !r.is_empty()).cloned().unwrap_or_else(|| "HEAD".into());
    if !load::ref_ok(&r) {
        return Err(bad(format!("`{r}` is not a branch, tag or commit")));
    }
    let sha = load::resolve(hub, &repo, &r).await.map_err(|e| internal(&e))?.ok_or_else(|| missing(format!("{} has no branch, tag or commit `{r}`", repo.name)))?;
    Ok((repo, r, sha))
}

fn counts(l: &Loaded) -> (usize, usize) {
    let errors = l.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    (errors + usize::from(l.workflow.is_none() && errors == 0), l.diagnostics.len() - errors)
}

/// `GET /api/ci/repos/:repo/workflows?ref=`: every workflow at the ref, whether each runs and
/// why GitHub's do or don't (W02-R02).
pub async fn list(hub: &Hub, repo: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let (repo, r, sha) = at_ref(hub, repo, q).await?;
    let commit = load::load(hub, &repo, &sha).await.map_err(|e| internal(&e))?;
    let workflows: Vec<serde_json::Value> = commit
        .workflows
        .iter()
        .map(|l| {
            let (errors, others) = counts(l);
            json!({
                "path": l.path,
                "dialect": l.dialect,
                "name": l.workflow.as_ref().and_then(|w| w.name.clone()).unwrap_or_else(|| l.path.clone()),
                "runs": l.runs,
                "errors": errors,
                "warnings": others,
                "outline": l.workflow.as_ref().map(wf::outline),
            })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "ref": r, "sha": sha, "github": { "run": commit.github_run, "why": commit.github_note }, "workflows": workflows })))
}

/// `GET /api/ci/repos/:repo/workflows/:path?ref=`: one workflow's source, model and diagnostics.
pub async fn get(hub: &Hub, repo: &str, path: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    if !matches!(wf::file_kind(path), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow)) {
        return Err(missing(format!("`{path}` is not where workflows are (.github/workflows/*.yml, .kiln/*.yaml)")));
    }
    let (repo, r, sha) = at_ref(hub, repo, q).await?;
    let commit = load::load(hub, &repo, &sha).await.map_err(|e| internal(&e))?;
    let l = commit.workflows.into_iter().find(|l| l.path == path).ok_or_else(|| missing(format!("no workflow `{path}` at {r} ({sha})")))?;
    let source = load::blob(hub, &repo, &sha, &l.path).await.map_err(|e| internal(&e))?;
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "path": l.path,
            "ref": r,
            "sha": sha,
            "dialect": l.dialect,
            "runs": l.runs,
            "github": { "run": commit.github_run, "why": commit.github_note },
            "source": source,
            "workflow": l.workflow,
            "diagnostics": l.diagnostics,
            "outline": l.workflow.as_ref().map(wf::outline),
        }),
    ))
}

#[derive(Deserialize)]
struct DispatchAsk {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/ci/repos/:repo/workflows/<path>/dispatches { ref, inputs }` (W03-R11): GitHub's
/// `workflow_dispatch`. 204 when the run starts.
pub async fn dispatch(hub: &Hub, repo: &str, path: &str, body: Incoming) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: DispatchAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    super::events::workflow_dispatch(hub, &repo, &ask.git_ref, path, &ask.inputs, "kiln").await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct RepoDispatchAsk {
    event_type: String,
    #[serde(default)]
    client_payload: serde_json::Value,
}

/// `POST /api/ci/repos/:repo/dispatches { event_type, client_payload }` (W03-R11): GitHub's
/// `repository_dispatch`. 204, as GitHub answers, whether or not a workflow listens.
pub async fn repository_dispatch(hub: &Hub, repo: &str, body: Incoming) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: RepoDispatchAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    super::events::repository_dispatch(hub, &repo, &ask.event_type, &ask.client_payload, "kiln").await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct NamedDispatchAsk {
    workflow: String,
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    inputs: serde_json::Map<String, serde_json::Value>,
}

/// `POST /api/ci/repos/:repo/dispatch { workflow, ref, inputs }` (§4.1's form of the same).
pub async fn dispatch_named(hub: &Hub, repo: &str, body: Incoming) -> Result<Response, Fail> {
    let repo = find_repo(hub, repo).await?;
    let ask: NamedDispatchAsk = srv::body_json(body, 1 << 20).await.map_err(|e| bad(format!("{e:#}")))?;
    super::events::workflow_dispatch(hub, &repo, &ask.git_ref, &ask.workflow, &ask.inputs, "kiln").await.map_err(|r| (r.status, r.message))?;
    Ok(srv::status(StatusCode::NO_CONTENT))
}
