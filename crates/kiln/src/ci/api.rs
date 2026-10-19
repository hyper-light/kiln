//! CI's HTTP surface (docs/ci.md §4, §2.6). Two entry points, called from `crate::api`:
//!
//! - [`open`]: routes that authenticate themselves before the console's auth applies: the GitHub
//!   runtime services (`/_kiln/…`, `/_apis/…`, `/twirp/…`, W08), job ID tokens and the OIDC issuer's discovery
//!   and keys (`/oidc/…`, W10), inbound webhooks (`/api/ci/hooks/:id`, W03). Answers `None` for
//!   paths it does not own, so the caller goes on.
//! - [`route`]: `/api/ci/…`, behind the console's auth.
//!
//! Each workstream adds its own arms, delegating to its own module.

use std::collections::HashMap;
use std::sync::Arc;

use http::{Method, StatusCode};

use super::{Fail, workflows};
use crate::http::server::{Request, Response};
use crate::hub::Hub;

/// Open routes; `None` when the path is not CI's.
pub async fn open(hub: &Arc<Hub>, seg: &[&str], req: Request) -> Option<Result<Response, Fail>> {
    if git_path(seg) {
        // W14: the code host legs fetch from, at the server's root, because `actions/checkout`
        // builds its remote from `GITHUB_SERVER_URL`'s origin only.
        return Some(super::facade::git(hub, seg, req).await);
    }
    match seg {
        ["_kiln", "git", rest @ ..] => Some(super::facade::git(hub, rest, req).await),
        // W14-R01: `/git/<owner>/<repo>.git`, where people clone and push native repositories.
        ["git", rest @ ..] if git_path(rest) => Some(super::facade::git(hub, rest, req).await),
        // W07: the run's pinned actions, for its legs.
        ["_kiln", "actions", rest @ ..] => Some(super::actions::tarball(hub, rest, &req).await),
        // W08: blobs, the cache (as GHES serves it) and the results service (artifacts).
        ["_kiln", "blob", id] => Some(super::services::blob::serve(hub, id, req).await),
        ["_apis", "artifactcache", rest @ ..] => Some(super::services::cache_v1::route(hub, rest, req).await),
        ["twirp", service, method] => Some(super::services::twirp::route(hub, service, method, req).await),
        // W14: GitHub's REST API (as GHES serves it, at /api/v3) and GraphQL, for a leg's token.
        ["api", "v3", rest @ ..] => Some(super::rest::route(hub, rest, req).await),
        ["api", "graphql"] => Some(super::rest::graphql(hub, req).await),
        ["api", "uploads", "repos", owner, repo, "releases", id, "assets"] => Some(super::rest::upload(hub, owner, repo, id, req).await),
        _ if super::rest::is_download(seg) => Some(super::rest::download(hub, seg, &req).await),
        _ if super::rest::is_web(seg) => Some(super::rest::web(hub, seg).await),
        // W10: ID tokens for jobs, and the issuer's documents.
        ["_kiln", "oidc", "token"] => Some(super::oidc::token(hub, &req).await),
        // W16: test reports from legs.
        ["_kiln", "test-results"] => Some(super::results::ingest(hub, req).await),
        // W19: a step's message (`kiln/notify@v1`).
        ["_kiln", "notify"] => Some(super::notify::from_leg(hub, req).await),
        // W19-R03: what a leg runs on, for comparing runs.
        ["_kiln", "facts"] => Some(super::compare::facts_from_leg(hub, req).await),
        // W18: an attest step's SBOM and bundles, for its runner; the key that signs them.
        ["_kiln", "attestations", "sbom"] if req.method() == Method::POST => Some(super::attest::upload(hub, req).await),
        ["_kiln", "attestations"] => Some(super::attest::for_leg(hub, &req).await),
        ["_kiln", "attestation-key.pem"] => Some(super::attest::key_pem(hub).await),
        // Only while tokens are minted (CI's jobs, CD's federated accounts): fetching the keys
        // makes the first one.
        _ if super::oidc::is_issuer(hub, seg).is_some() => {
            let doc = super::oidc::is_issuer(hub, seg).unwrap_or_default();
            Some(if super::oidc::active(hub).await { super::oidc::document(hub, doc).await } else { Err((StatusCode::NOT_FOUND, "not found".into())) })
        }
        ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..] => Some(Err((StatusCode::NOT_FOUND, "not found".into()))),
        _ => None,
    }
}

/// Is this path one of [`open`]'s? (So the caller can hand the request over without cloning it.)
pub fn is_open(seg: &[&str]) -> bool {
    git_path(seg) || matches!(seg, ["git", rest @ ..] if git_path(rest)) || super::rest::is_web(seg) || super::rest::is_download(seg) || matches!(seg, ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..] | ["api", "v3", ..] | ["api", "graphql"] | ["api", "uploads", ..])
}

/// First segments that are kiln's own, never a repository owner.
pub fn reserved(owner: &str) -> bool {
    matches!(owner, "api" | "_kiln" | "_apis" | "oidc" | "twirp" | "assets" | "dl" | "runs" | "repos" | "agents" | "runners" | "deployments" | "settings" | "system")
}

/// git's smart HTTP for a repository: `/<owner>/<repo>[.git]/info/refs`, `…/git-upload-pack`
/// and `…/git-receive-pack` (native repositories take pushes, W14-R02).
fn git_path(seg: &[&str]) -> bool {
    match seg {
        [owner, _, "info", "refs"] | [owner, _, "git-upload-pack"] | [owner, _, "git-receive-pack"] => !reserved(owner),
        _ => false,
    }
}

/// `/api/ci/<rest>`, authenticated.
/// Whether a call may reach other hosts (docs/ci.md W23): a repository's CI providers, a provider's
/// runs, logs, artifacts and reviews, a host's pull requests.
fn reaches_out(method: &Method, rest: &[&str]) -> bool {
    matches!(
        (method, rest),
        (&Method::GET | &Method::PUT, ["repos", _, "providers"])
            | (_, ["repos", _, "providers", "sync" | "workflows"])
            | (&Method::GET, ["repos", _, "commits", _, "checks"])
            | (_, ["repos", _, "pulls", ..])
            | (&Method::POST, ["repos", _, "workflows", .., "dispatches"])
            | (&Method::GET, ["runs", _, "pending" | "artifacts"])
            | (&Method::GET, ["runs", _, "artifacts", _, "download"])
            | (&Method::POST, ["runs", _, "cancel" | "rerun" | "approve"])
            | (&Method::GET, ["legs", _, "logs"])
            | (&Method::GET, ["environments"])
    )
}

/// A call that may reach other hosts, as a task of its own: a host's API calls nest deep, and a
/// request's stack is already spent on the route tables above it (a debug build gives every arm's
/// future a slot of its own).
async fn reach_out(hub: &Arc<Hub>, method: Method, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let caller = req.extensions().get::<crate::authz::Caller>().cloned();
    let body: serde_json::Value = if matches!(method, Method::POST | Method::PUT) { crate::http::server::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
    let (hub, rest, q) = (Arc::clone(hub), rest.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>(), q.clone());
    crate::rt::spawn(async move {
        let segs: Vec<&str> = rest.iter().map(String::as_str).collect();
        outside(&hub, &method, &segs, &q, body, caller.as_ref()).await
    })
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "the call ended without an answer".to_owned()))?
}

async fn outside(hub: &Arc<Hub>, method: &Method, rest: &[&str], q: &HashMap<String, String>, body: serde_json::Value, caller: Option<&crate::authz::Caller>) -> Result<Response, Fail> {
    let ok = |v: &serde_json::Value| crate::http::server::json(StatusCode::OK, v);
    match (method, rest) {
        // W23: what a repository's CI runs on, and the rest of the providers' contract (R01).
        (&Method::GET, ["repos", repo, "providers"]) => Ok(ok(&super::providers::show(hub, repo).await?)),
        (&Method::PUT, ["repos", repo, "providers"]) => {
            let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            Ok(ok(&super::providers::set(hub, repo, &body, &by).await?))
        }
        (&Method::POST, ["repos", repo, "providers", "sync"]) => Ok(ok(&super::providers::sync_now(hub, repo).await?)),
        (&Method::GET, ["repos", repo, "providers", "workflows"]) => Ok(ok(&super::providers::workflows(hub, repo).await?)),
        (&Method::GET, ["repos", repo, "commits", sha, "checks"]) => Ok(ok(&super::providers::checks(hub, repo, sha).await?)),
        // W23-R06: the host's pull requests (merge requests).
        (_, ["repos", repo, "pulls", tail @ ..]) => crate::githost::pulls::route(hub, method, repo, tail, body, caller).await,
        // W03-R11: dispatches (another provider's, W23-R01).
        (&Method::POST, ["repos", repo, "workflows", path @ .., "dispatches"]) => workflows::dispatch_value(hub, repo, &path.join("/"), body).await,
        (&Method::GET, ["runs", id, "pending"]) => super::runs_api::pending(hub, id).await,
        (&Method::GET, ["runs", id, "artifacts"]) => super::runs_api::part(hub, id, "artifacts", q).await,
        (&Method::GET, ["runs", id, "artifacts", artifact, "download"]) => super::runs_api::download(hub, id, artifact).await,
        (&Method::POST, ["runs", id, "cancel"]) => super::runs_api::cancel(hub, id).await,
        (&Method::POST, ["runs", id, "rerun"]) => super::runs_api::rerun(hub, id, &body).await,
        (&Method::POST, ["runs", id, "approve"]) => super::environments::approve(hub, id, body, caller).await,
        (&Method::GET, ["legs", id, "logs"]) => super::runs_api::logs(hub, id, q).await,
        (&Method::GET, ["environments"]) => super::environments::route(hub, method, &[], q, body, caller).await,
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

pub async fn route(hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let method = req.method().clone();
    if reaches_out(&method, rest) {
        return reach_out(hub, method, rest, q, req).await;
    }
    match (&method, rest) {
        // W14-R10: the GitHub App, and making one by GitHub's manifest flow.
        (&Method::GET, ["github", "app"]) => crate::githost::web::show(hub).await,
        (&Method::POST, ["github", "app", "manifest"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 20).await.unwrap_or(serde_json::Value::Null);
            crate::githost::web::start(hub, &by, &body).await
        }
        // W02: workflows.
        (&Method::POST, ["validate"]) => workflows::validate(req.into_body()).await,
        // W04-R29: a plan without a run.
        (&Method::POST, ["plan-preview"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            super::preview::route(hub, caller.as_ref(), req.into_body()).await
        }
        (&Method::GET, ["schema", kind]) => workflows::schema(kind),
        (&Method::GET, ["repos", repo, "workflows"]) => workflows::list(hub, repo, q).await,
        (&Method::GET, ["repos", repo, "workflows", path @ ..]) => workflows::get(hub, repo, &path.join("/"), q).await,
        // W10-R09: subjects per organization and repository.
        (_, ["oidc", "subject"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = if method == Method::PUT { crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            super::oidc::subject(hub, &method, q, body, &by).await
        }
        // W10-R12: the OIDC issuer's status, keys to publish, rotation.
        (_, ["oidc", tail @ ..]) => {
            let body: serde_json::Value = if method == Method::POST { crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            super::oidc::admin(hub, &method, tail, body).await
        }
        // W07-R04, R12: action policies, the catalog, prefetching.
        (_, ["actions", tail @ ..]) => {
            let body: serde_json::Value = if method == Method::PUT || method == Method::POST { crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            super::actions::route(hub, &method, tail, q, body).await
        }
        // W15: environments, their deployments, and reviews of runs waiting on them.
        (_, ["environments", tail @ ..]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = if matches!(method, Method::POST | Method::PUT | Method::PATCH) { crate::http::server::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            super::environments::route(hub, &method, tail, q, body, caller.as_ref()).await
        }
        // W13-R11: the runner images kiln built.
        (&Method::GET, ["images"]) => super::images::list(hub, q).await,
        // W13-R09: pools and their executors.
        (&Method::GET, ["pools"]) => super::pools::list(hub, None).await,
        (&Method::GET, ["pools", name]) => super::pools::list(hub, Some(name)).await,
        (&Method::PUT, ["pools", name]) => {
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 64 << 10).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::pools::put(hub, name, &body).await
        }
        (&Method::DELETE, ["pools", name]) => super::pools::delete(hub, name).await,
        // W18-R04: whether a digest has kiln's provenance meeting a requirement.
        (&Method::POST, ["attestations", "verify"]) => {
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::attest::verify_route(hub, &body).await
        }
        // W05-R21: routing rules, and where a leg would go.
        (&Method::GET, ["routing", "rules"]) => super::routing::get(hub).await,
        (&Method::PUT, ["routing", "rules"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 20).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::routing::put(hub, q, body, &by).await
        }
        (&Method::POST, ["routing", "explain"]) => {
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::routing::explain(hub, body).await
        }
        // W09: secrets (names only) and variables.
        (_, ["secrets", ..] | ["variables", ..]) => super::secrets::route(hub, &method, rest, q, req.into_body()).await,
        // W11: integrations, shared by jobs and deployments (docs/cd-targets.md §6, §7.1).
        (_, ["integrations", ..]) => super::integrations::route(hub, &method, rest, q, req.into_body()).await,
        // §4.1: the engine's runs, legs and caches (W21's data).
        (&Method::GET, ["runs"]) => super::runs_api::list(hub, q).await,
        (&Method::GET, ["runs", id]) => super::runs_api::get(hub, id, q).await,
        (&Method::GET, ["runs", id, "graph"]) => super::runs_api::graph(hub, id, q).await,
        // W19-R03: what changed between two runs or legs (`last-green`: since the last success).
        (&Method::GET, ["runs", id, "compare", other]) => super::compare::runs(hub, id, other).await,
        (&Method::GET, ["runs", id, "attestations"]) => super::attest::for_run(hub, id).await,
        (&Method::GET, ["legs", id, "compare", other]) => super::compare::legs(hub, id, other).await,
        // W16-R03: tests, failures first.
        (&Method::GET, ["runs", id, "tests"]) => super::results::of(hub, Some(id), None, q).await,
        (&Method::GET, ["legs", id, "tests"]) => super::results::of(hub, None, Some(id), q).await,
        (&Method::GET, ["repos", repo, "tests"]) => super::results::repo(hub, repo, q).await,
        // W19-R06: what notifications went out.
        (&Method::GET, ["notify", "deliveries"]) => super::notify::deliveries(hub, q).await,
        // W19-R04: log search across runs, and within a leg.
        (&Method::GET, ["logs", "search"]) => super::logsearch::route(hub, q).await,
        (&Method::GET, ["legs", id, "logs", what @ ("search" | "failure")]) => super::logsearch::leg(hub, id, what, q).await,
        // W19-R01: insights.
        (&Method::GET, ["insights", rest @ ..]) => super::insights::route(hub, rest, q).await,
        // W19-R07: outgoing webhooks' deliveries, and sending one again.
        (_, ["webhooks", rest @ ..]) => super::webhooks::route(hub, &method, rest, q).await,
        // W16-R07, R08: benchmark metrics and their trends.
        (&Method::GET, ["runs", id, "metrics"]) => super::results::run_metrics(hub, id).await,
        (&Method::GET, ["repos", repo, "metrics"]) => super::results::repo_metrics(hub, repo, q).await,
        (&Method::GET, ["runs", id, which @ ("annotations" | "summary")]) => super::runs_api::part(hub, id, which, q).await,
        (&Method::GET, ["legs", id]) => super::runs_api::leg(hub, id).await,
        (&Method::GET, ["legs", id, "routing"]) => super::runs_api::routing(hub, id).await,
        (&Method::POST, ["legs", id, "rerun-step"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::runs_api::rerun_step(hub, id, &body, &by).await
        }
        (&Method::POST, ["legs", id, "exec"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::runs_api::exec(hub, id, &body, caller.as_ref()).await
        }
        (&Method::POST, ["legs", id, "release"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            super::runs_api::release(hub, id, &by).await
        }
        (&Method::POST, ["legs", id, "keep"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            super::runs_api::extend(hub, id, &body, &by).await
        }
        (&Method::GET, ["repos", repo, "caches"]) => super::runs_api::caches(hub, repo, None).await,
        // W21-R01: the secret and variable names a workflow sees (names only), for completion.
        (&Method::GET, ["repos", repo, "names"]) => {
            let path = q.get("path").cloned().unwrap_or_default();
            let repo = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.id == *repo || r.name == *repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
            let secrets = super::secrets::names_for(hub, &repo, &path, None).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let vars = super::secrets::vars_for(hub, &repo, &path, None).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(crate::http::server::json(StatusCode::OK, &serde_json::json!({ "secrets": secrets, "variables": vars.keys().collect::<Vec<_>>() })))
        }
        (&Method::DELETE, ["repos", repo, "caches", id]) => super::runs_api::caches(hub, repo, Some(id)).await,
        // §4.1's names for the dispatches.
        (&Method::POST, ["repos", repo, "dispatch"]) => workflows::dispatch_named(hub, repo, req.into_body()).await,
        (&Method::POST, ["repos", repo, "repository-dispatch"]) => workflows::repository_dispatch(hub, repo, req.into_body()).await,
        // W03-R11: dispatches.
        (&Method::POST, ["repos", repo, "dispatches"]) => workflows::repository_dispatch(hub, repo, req.into_body()).await,
        // W17-R06: runs of a working tree (`kiln workflow run`).
        (&Method::GET, ["repos", repo, "heads"]) => super::uploads::heads(hub, repo).await,
        (&Method::POST, ["repos", repo, "packs"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            super::uploads::pack(hub, repo, q, req.into_body(), &by).await
        }
        (&Method::POST, ["repos", repo, "local-runs"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            super::uploads::local_runs(hub, repo, caller.as_ref(), req.into_body()).await
        }
        _ => Err((StatusCode::NOT_FOUND, format!("no such CI route: {method} /api/ci/{}", rest.join("/")))),
    }
}
