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
use crate::api::boxed;
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
        // W08-R08: artifacts as v1 to v3 of the artifact actions keep them.
        ["_apis", "pipelines", "workflows", _, "artifacts"] => Some(super::services::pipelines::artifacts(hub, req).await),
        ["_apis", "resources", "Containers", id] => Some(super::services::pipelines::container(hub, id, req).await),
        // W15-R05: `kiln/deploy@v1`'s releases.
        ["_kiln", "deploy", rest @ ..] => Some(super::deploy_step::route(hub, rest, req).await),
        // W08-R09: kiln's chunked cache, for `kiln/cache@v1`.
        ["_kiln", "cache", rest @ ..] => Some(super::services::chunks::route(hub, rest, req).await),
        // W24-R07: the work ledger, for steps' reuse.
        ["_kiln", "work", "v1", rest @ ..] => Some(super::services::memo::route(hub, rest, req).await),
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
        ["_kiln", "notify"] => {
            let hub = Arc::clone(hub);
            Some(apart(async move { super::notify::from_leg(&hub, req).await }).await)
        }
        // W19-R03: what a leg runs on, for comparing runs.
        ["_kiln", "facts"] => Some(super::compare::facts_from_leg(hub, req).await),
        // W18: an attest step's SBOM and bundles, for its runner; the key that signs them.
        ["_kiln", "attestations", "sbom"] if req.method() == Method::POST => Some(super::attest::upload(hub, req).await),
        ["_kiln", "attestations"] => Some(super::attest::for_leg(hub, &req).await),
        ["_kiln", "attestation-key.pem"] => Some(super::attest::key_pem(hub).await),
        // W03-R02: an inbound webhook's call, checked by its signature.
        ["api", "ci", "hooks", id] if req.method() == Method::POST => Some(super::inbound::call(hub, id, req).await),
        // W14-R05: Slack's Events API, at a Slack integration's own address.
        ["api", "ci", "replies", "slack", id, key] if req.method() == Method::POST => {
            let (hub, id, key) = (Arc::clone(hub), (*id).to_owned(), (*key).to_owned());
            Some(apart(async move { super::replies::slack_events(&hub, &id, &key, req).await }).await)
        }
        // docs/deploy-api.md §12.3: what a chat sends kiln (commands, buttons, events), at an
        // integration's own address, checked as that chat signs it.
        ["api", "ci", "chat", kind, id, key] if req.method() == Method::POST => {
            let (hub, kind, id, key) = (Arc::clone(hub), (*kind).to_owned(), (*id).to_owned(), (*key).to_owned());
            Some(apart(async move { super::chat::inbound(&hub, &kind, &id, &key, req).await }).await)
        }
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
    git_path(seg)
        || matches!(seg, ["git", rest @ ..] if git_path(rest))
        || super::rest::is_web(seg)
        || super::rest::is_download(seg)
        || matches!(
            seg,
            ["_kiln", ..]
                | ["_apis", ..]
                | ["oidc", ..]
                | ["twirp", ..]
                | ["api", "v3", ..]
                | ["api", "graphql"]
                | ["api", "uploads", ..]
                | ["api", "ci", "hooks", _]
                | ["api", "ci", "replies", "slack", _, _]
                | ["api", "ci", "chat", _, _, _]
        )
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
            | (&Method::POST, ["repos", _, "workflows", .., "dispatches" | "edit"])
            | (&Method::GET, ["runs", _, "pending" | "artifacts"])
            | (&Method::GET, ["runs", _, "artifacts", _, "download"])
            | (&Method::POST, ["runs", _, "cancel" | "rerun" | "approve"])
            | (&Method::POST, ["legs", _, "retry"])
            | (&Method::GET, ["legs", _, "logs"])
            | (&Method::GET, ["environments"])
    )
}

/// `f` on a task of its own: a call that reaches other hosts (a webhook, KMS, a chat service)
/// nests deep, and beneath the router's frames a debug build's shard stack runs out.
async fn apart<F>(f: F) -> Result<Response, Fail>
where
    F: std::future::Future<Output = Result<Response, Fail>> + Send + 'static,
{
    crate::rt::spawn(f).await.map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "the call ended without an answer".to_owned()))?
}

fn owned(rest: &[&str]) -> Vec<String> {
    rest.iter().map(|s| (*s).to_owned()).collect()
}

/// A call that may reach other hosts, as a task of its own: a host's API calls nest deep, and a
/// request's stack is already spent on the route tables above it (a debug build gives every arm's
/// future a slot of its own).
async fn reach_out(hub: &Arc<Hub>, method: Method, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let caller = req.extensions().get::<crate::authz::Caller>().cloned();
    // A workflow's new text is up to 1 MiB (W02-R03), and JSON's escapes on top.
    let limit = if matches!(rest, ["repos", _, "workflows", .., "edit"]) { 2 << 20 } else { 1 << 20 };
    let body: serde_json::Value = if matches!(method, Method::POST | Method::PUT) { crate::http::server::body_json(req.into_body(), limit).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
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
        // W02-R12, W21-R06: a workflow's new text, saved as its repository takes it.
        (&Method::POST, ["repos", repo, "workflows", path @ .., "edit"]) => {
            let caller = caller.cloned().unwrap_or_else(crate::authz::Caller::owner);
            Ok(ok(&super::workflows::edit(hub, &caller, repo, &path.join("/"), &body).await?))
        }
        // W23: what a repository's CI runs on, and the rest of the providers' contract (R01).
        (&Method::GET, ["repos", repo, "providers"]) => Ok(ok(&super::providers::show(hub, repo).await?)),
        (&Method::PUT, ["repos", repo, "providers"]) => {
            let by = caller.map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            Ok(ok(&super::providers::set(hub, repo, &body, &by).await?))
        }
        (&Method::POST, ["repos", repo, "providers", "sync"]) => Ok(ok(&super::providers::sync_now(hub, repo).await?)),
        (&Method::GET, ["repos", repo, "providers", "workflows"]) => Ok(ok(&super::providers::workflows(hub, repo).await?)),
        (&Method::GET, ["repos", repo, "commits", sha, "checks"]) => Ok(ok(&super::providers::checks(hub, repo, sha).await?)),
        // W26-R07: a search across a host's pull request, at its mirrored head.
        (&Method::GET, ["repos", repo, "pulls", n, "search"]) => Ok(ok(&super::search::pull(hub, repo, n, q, caller).await?)),
        // W23-R06: the host's pull requests (merge requests).
        (_, ["repos", repo, "pulls", tail @ ..]) => crate::githost::pulls::route(hub, method, repo, tail, body, caller).await,
        // W03-R11: dispatches (another provider's, W23-R01).
        (&Method::POST, ["repos", repo, "workflows", path @ .., "dispatches"]) => workflows::dispatch_value(hub, repo, &path.join("/"), body, caller.and_then(crate::authz::Caller::started_by)).await,
        (&Method::GET, ["runs", id, "pending"]) => super::runs_api::pending(hub, id).await,
        (&Method::GET, ["runs", id, "artifacts"]) => super::runs_api::part(hub, id, "artifacts", q).await,
        (&Method::GET, ["runs", id, "artifacts", artifact, "download"]) => super::runs_api::download(hub, id, artifact).await,
        (&Method::POST, ["runs", id, "cancel"]) => super::runs_api::cancel(hub, id).await,
        (&Method::POST, ["runs", id, "rerun"]) => super::runs_api::rerun(hub, id, &body, caller.map_or("admin", |c| c.name.as_str())).await,
        (&Method::POST, ["runs", id, "approve"]) => super::environments::approve(hub, id, body, caller).await,
        // W05-R28: a finished job again, here or elsewhere (another provider's, by its provider).
        (&Method::POST, ["legs", id, "retry"]) => super::retry::retry(hub, id, &body, caller.map_or("admin", |c| c.name.as_str())).await,
        (&Method::GET, ["legs", id, "logs"]) => super::runs_api::logs(hub, id, q).await,
        (&Method::GET, ["environments"]) => super::environments::route(hub, method, &[], q, body, caller).await,
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

pub async fn route(hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let method = req.method().clone();
    if reaches_out(&method, rest) {
        return boxed(|| reach_out(hub, method, rest, q, req)).await;
    }
    match (&method, rest) {
        // W14-R10: the GitHub App, and making one by GitHub's manifest flow.
        (&Method::GET, ["github", "app"]) => boxed(|| crate::githost::web::show(hub)).await,
        (&Method::POST, ["github", "app", "manifest"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 20)).await.unwrap_or(serde_json::Value::Null);
            boxed(|| crate::githost::web::start(hub, &by, &body)).await
        }
        // W02: workflows.
        (&Method::POST, ["validate"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            boxed(|| workflows::validate(hub, caller.as_ref(), req.into_body())).await
        }
        // W13-R11: an image's definition, checked; or proposed for the repository ("Save to repo").
        (&Method::POST, ["images", "validate"]) => boxed(|| super::images::validate(req.into_body())).await,
        // W21-R08: an image's definition, as its editor has it, built once (and pushed, the owner's).
        (&Method::POST, ["images", "build"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 2 << 20)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| async { super::uploads::build(hub, caller.as_ref(), q, body).await.map(|v| crate::http::server::json(StatusCode::CREATED, &v)) }).await
        }
        (&Method::POST, ["images"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned().unwrap_or_else(crate::authz::Caller::owner);
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 20)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::images::propose(hub, q, &caller, &body)).await
        }
        // W04-R29: a plan without a run.
        (&Method::POST, ["plan-preview"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            boxed(|| super::preview::route(hub, caller.as_ref(), req.into_body())).await
        }
        (&Method::GET, ["schema", kind]) => workflows::schema(kind),
        (&Method::GET, ["repos", repo, "workflows"]) => boxed(|| workflows::list(hub, repo, q)).await,
        (&Method::GET, ["repos", repo, "labels"]) => boxed(|| workflows::labels(hub, repo)).await,
        (&Method::GET, ["repos", repo, "workflows", path @ ..]) => {
            let path = path.join("/");
            boxed(|| workflows::get(hub, repo, &path, q)).await
        }
        // W10-R09: subjects per organization and repository.
        (_, ["oidc", "subject"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = if method == Method::PUT { boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            boxed(|| super::oidc::subject(hub, &method, q, body, &by)).await
        }
        // W10-R11: an agent kiln started asks for its own workload token.
        (&Method::POST, ["oidc", "token"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned().unwrap_or_else(crate::authz::Caller::owner);
            let body: serde_json::Value = crate::http::server::body_json(req.into_body(), 1 << 16).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            Ok(crate::http::server::json(StatusCode::OK, &super::oidc::agent_token(hub, &caller, &body).await?))
        }
        // W10-R12: the OIDC issuer's status, keys to publish, rotation.
        (_, ["oidc", tail @ ..]) => {
            let body: serde_json::Value = if method == Method::POST { boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            boxed(|| super::oidc::admin(hub, &method, tail, body)).await
        }
        // W07-R04, R12: action policies, the catalog, prefetching.
        (_, ["actions", tail @ ..]) => {
            let body: serde_json::Value = if method == Method::PUT || method == Method::POST { boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            boxed(|| super::actions::route(hub, &method, tail, q, body)).await
        }
        // W15: environments, their deployments, and reviews of runs waiting on them.
        (_, ["environments", tail @ ..]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = if matches!(method, Method::POST | Method::PUT | Method::PATCH) { boxed(|| crate::http::server::body_json(req.into_body(), 1 << 20)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))? } else { serde_json::Value::Null };
            boxed(|| super::environments::route(hub, &method, tail, q, body, caller.as_ref())).await
        }
        // W13-R11: the runner images kiln built.
        (&Method::GET, ["images"]) => boxed(|| super::images::list(hub, q)).await,
        (&Method::GET, ["registry", "mirrors"]) => boxed(|| super::registry::mirror::mirrors(hub)).await,
        // W13-R09: pools and their executors.
        (&Method::GET, ["pools"]) => boxed(|| super::pools::list(hub, None)).await,
        (&Method::GET, ["pools", name]) => boxed(|| super::pools::list(hub, Some(name))).await,
        (&Method::GET, ["pools", name, "history"]) => boxed(|| super::pools::history(hub, name, q)).await,
        (&Method::PUT, ["pools", name]) => {
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 64 << 10)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::pools::put(hub, name, &body)).await
        }
        (&Method::DELETE, ["pools", name]) => boxed(|| super::pools::delete(hub, name)).await,
        // W18-R04: whether a digest has kiln's provenance meeting a requirement.
        (&Method::POST, ["attestations", "verify"]) => {
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            let hub = Arc::clone(hub);
            apart(async move { super::attest::verify_route(&hub, &body).await }).await
        }
        // W08-R01: the store verified (each chunk re-hashed) or collected.
        (&Method::POST, ["store", "verify"]) => {
            let v = boxed(|| super::services::cas::verify(hub)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(crate::http::server::json(StatusCode::OK, &serde_json::to_value(v).unwrap_or_default()))
        }
        (&Method::POST, ["store", "gc"]) => {
            let c = boxed(|| super::services::cas::collect(hub)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(crate::http::server::json(StatusCode::OK, &serde_json::json!({ "objects": c.objects, "bytes": c.bytes })))
        }
        // W24-R31: the reaper's policies.
        (&Method::GET, ["work", "reap", "policies"]) => boxed(|| super::reap::get(hub, q)).await,
        (&Method::PUT, ["work", "reap", "policies"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::reap::put(hub, q, body, caller.as_ref())).await
        }
        // W05-R21: routing rules, and where a leg would go.
        (&Method::GET, ["routing", "rules"]) => boxed(|| super::routing::get(hub)).await,
        (&Method::PUT, ["routing", "rules"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 20)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::routing::put(hub, q, body, &by)).await
        }
        (&Method::POST, ["routing", "explain"]) => {
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::routing::explain(hub, body)).await
        }
        // W09: secrets (names only) and variables.
        (_, ["secrets", ..] | ["variables", ..]) => boxed(|| super::secrets::route(hub, &method, rest, q, req.into_body())).await,
        // W11: integrations, shared by jobs and deployments (docs/cd-targets.md §6, §7.1).
        (_, ["integrations", ..]) => {
            let (hub, method, rest, q, body) = (Arc::clone(hub), method.clone(), owned(rest), q.clone(), req.into_body());
            apart(async move { super::integrations::route(&hub, &method, &rest.iter().map(String::as_str).collect::<Vec<_>>(), &q, body).await }).await
        }
        // §4.1: the engine's runs, legs and caches (W21's data).
        (&Method::GET, ["runs"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            boxed(|| super::runs_api::list(hub, q, caller.as_ref())).await
        }
        (&Method::GET, ["runs", id]) => boxed(|| super::runs_api::get(hub, id, q)).await,
        (&Method::GET, ["runs", id, "graph"]) => boxed(|| super::runs_api::graph(hub, id, q)).await,
        // W20: a failure bisected, the run's job run on the commits since it last passed.
        (&Method::POST, ["runs", id, "bisect"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.unwrap_or(serde_json::Value::Null);
            boxed(|| super::bisect::start(hub, id, &body, &by)).await
        }
        (&Method::GET, ["runs", id, "bisects"]) => boxed(|| super::bisect::list(hub, id, None)).await,
        (&Method::GET, ["runs", id, "bisects", b]) => boxed(|| super::bisect::list(hub, id, Some(b))).await,
        (&Method::GET, ["runs", id, "flow"]) => boxed(|| super::runs_api::flow(hub, id, q)).await,
        // W19-R03: what changed between two runs or legs (`last-green`: since the last success).
        (&Method::GET, ["runs", id, "compare", other]) => boxed(|| super::compare::runs(hub, id, other)).await,
        (&Method::GET, ["runs", id, "attestations"]) => boxed(|| super::attest::for_run(hub, id)).await,
        (&Method::GET, ["legs", id, "compare", other]) => boxed(|| super::compare::legs(hub, id, other)).await,
        // W16-R03: tests, failures first.
        (&Method::GET, ["runs", id, "tests"]) => boxed(|| super::results::of(hub, Some(id), None, q)).await,
        (&Method::GET, ["legs", id, "tests"]) => boxed(|| super::results::of(hub, None, Some(id), q)).await,
        (&Method::GET, ["repos", repo, "tests"]) => boxed(|| super::results::repo(hub, repo, q)).await,
        (&Method::GET, ["repos", repo, "tests", id, "history"]) => boxed(|| super::results::history(hub, repo, id, q)).await,
        // W19-R06: what notifications went out.
        (&Method::GET, ["notify", "deliveries"]) => boxed(|| super::notify::deliveries(hub, q)).await,
        // W19-R04: log search across runs, and within a leg.
        (&Method::GET, ["logs", "search"]) => boxed(|| super::logsearch::route(hub, q)).await,
        (&Method::GET, ["legs", id, "logs", what @ ("search" | "failure")]) => boxed(|| super::logsearch::leg(hub, id, what, q)).await,
        // W19-R01: insights.
        (&Method::GET, ["insights", rest @ ..]) => boxed(|| super::insights::route(hub, rest, q)).await,
        // W19-R07: outgoing webhooks' deliveries, and sending one again.
        (_, ["webhooks", rest @ ..]) => {
            let (hub, method, rest, q) = (Arc::clone(hub), method.clone(), owned(rest), q.clone());
            apart(async move { super::webhooks::route(&hub, &method, &rest.iter().map(String::as_str).collect::<Vec<_>>(), &q).await }).await
        }
        // W16-R07, R08: benchmark metrics and their trends.
        (&Method::GET, ["runs", id, "metrics"]) => boxed(|| super::results::run_metrics(hub, id)).await,
        (&Method::GET, ["repos", repo, "metrics"]) => boxed(|| super::results::repo_metrics(hub, repo, q)).await,
        (&Method::GET, ["runs", id, which @ ("annotations" | "summary")]) => boxed(|| super::runs_api::part(hub, id, which, q)).await,
        (&Method::GET, ["legs", id]) => boxed(|| super::runs_api::leg(hub, id)).await,
        (&Method::GET, ["legs", id, "routing"]) => boxed(|| super::runs_api::routing(hub, id)).await,
        (&Method::GET, ["legs", id, "runners"]) => boxed(|| super::retry::runners(hub, id)).await,
        (&Method::POST, ["legs", id, "rerun-step"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::runs_api::rerun_step(hub, id, &body, &by)).await
        }
        (&Method::POST, ["legs", id, "exec"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::runs_api::exec(hub, id, &body, caller.as_ref())).await
        }
        (&Method::POST, ["legs", id, "release"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            boxed(|| super::runs_api::release(hub, id, &by)).await
        }
        (&Method::POST, ["legs", id, "keep"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 16)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| super::runs_api::extend(hub, id, &body, &by)).await
        }
        (&Method::GET, ["repos", repo, "caches"]) => boxed(|| super::runs_api::caches(hub, repo, None)).await,
        // W21-R01: the secret and variable names a workflow sees (names only), for completion.
        (&Method::GET, ["repos", repo, "names"]) => {
            let path = q.get("path").cloned().unwrap_or_default();
            let repo = boxed(|| hub.store.repos()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.id == *repo || r.name == *repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))?;
            let secrets = boxed(|| super::secrets::names_for(hub, &repo, &path, None)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let vars = boxed(|| super::secrets::vars_for(hub, &repo, &path, None)).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            Ok(crate::http::server::json(StatusCode::OK, &serde_json::json!({ "secrets": secrets, "variables": vars.keys().collect::<Vec<_>>() })))
        }
        (&Method::DELETE, ["repos", repo, "caches", id]) => boxed(|| super::runs_api::caches(hub, repo, Some(id))).await,
        // §4.1's names for the dispatches.
        (&Method::POST, ["repos", repo, "dispatch"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().and_then(crate::authz::Caller::started_by);
            boxed(|| workflows::dispatch_named(hub, repo, req.into_body(), by)).await
        }
        (&Method::POST, ["repos", repo, "repository-dispatch"] | ["repos", repo, "dispatches"]) => {
            // W03-R11: dispatches.
            let by = req.extensions().get::<crate::authz::Caller>().and_then(crate::authz::Caller::started_by);
            boxed(|| workflows::repository_dispatch(hub, repo, req.into_body(), by)).await
        }
        // W03-R02: inbound webhooks (`/api/ci/hooks/:id` is their call).
        (&Method::GET, ["repos", repo, "hooks"]) => boxed(|| super::inbound::list(hub, repo)).await,
        (&Method::POST, ["repos", repo, "hooks"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 1 << 20)).await.unwrap_or(serde_json::Value::Null);
            boxed(|| super::inbound::create(hub, repo, &body, &by)).await
        }
        (&Method::DELETE, ["repos", repo, "hooks", id]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            boxed(|| super::inbound::delete(hub, repo, id, &by)).await
        }
        // W03-R09: kiln's `agent` event, the caller the agent asking.
        (&Method::POST, ["repos", repo, "agent"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "kiln".to_owned(), |c| c.name.clone());
            let started_by = req.extensions().get::<crate::authz::Caller>().and_then(crate::authz::Caller::started_by);
            boxed(|| workflows::agent_run(hub, repo, &by, req.into_body(), started_by)).await
        }
        // W17-R06: runs of a working tree (`kiln workflow run`).
        (&Method::GET, ["repos", repo, "heads"]) => boxed(|| super::uploads::heads(hub, repo)).await,
        (&Method::POST, ["repos", repo, "packs"]) => {
            let by = req.extensions().get::<crate::authz::Caller>().map_or_else(|| "admin".to_owned(), |c| c.name.clone());
            boxed(|| super::uploads::pack(hub, repo, q, req.into_body(), &by)).await
        }
        (&Method::POST, ["repos", repo, "local-runs"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            boxed(|| super::uploads::local_runs(hub, repo, caller.as_ref(), req.into_body())).await
        }
        // W21-R07: an action, as its editor has it, tried once.
        (&Method::POST, ["repos", repo, "actions", "try"]) => {
            let caller = req.extensions().get::<crate::authz::Caller>().cloned();
            let body: serde_json::Value = boxed(|| crate::http::server::body_json(req.into_body(), 2 << 20)).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            boxed(|| async { super::uploads::try_action(hub, caller.as_ref(), repo, body).await.map(|v| crate::http::server::json(StatusCode::CREATED, &v)) }).await
        }
        _ => Err((StatusCode::NOT_FOUND, format!("no such CI route: {method} /api/ci/{}", rest.join("/")))),
    }
}
