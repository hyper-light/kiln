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
        // W07: the run's pinned actions, for its legs.
        ["_kiln", "actions", rest @ ..] => Some(super::actions::tarball(hub, rest, &req).await),
        // W08: blobs, the cache (as GHES serves it) and the results service (artifacts).
        ["_kiln", "blob", id] => Some(super::services::blob::serve(hub, id, req).await),
        ["_apis", "artifactcache", rest @ ..] => Some(super::services::cache_v1::route(hub, rest, req).await),
        ["twirp", service, method] => Some(super::services::twirp::route(hub, service, method, req).await),
        // W14: GitHub's REST API (as GHES serves it, at /api/v3) and GraphQL, for a leg's token.
        ["api", "v3", rest @ ..] => Some(super::rest::route(hub, rest, req).await),
        ["api", "graphql"] => Some(super::rest::graphql(hub, req).await),
        _ if super::rest::is_web(seg) => Some(super::rest::web(hub, seg).await),
        // W10: ID tokens for jobs, and the issuer's documents.
        ["_kiln", "oidc", "token"] => Some(super::oidc::token(hub, &req).await),
        // Only while the engine runs: fetching the keys makes the first one.
        _ if hub.ci.enabled() && super::oidc::is_issuer(hub, seg).is_some() => Some(super::oidc::document(hub, super::oidc::is_issuer(hub, seg).unwrap_or_default()).await),
        ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..] => Some(Err((StatusCode::NOT_FOUND, "not found".into()))),
        _ => None,
    }
}

/// Is this path one of [`open`]'s? (So the caller can hand the request over without cloning it.)
pub fn is_open(seg: &[&str]) -> bool {
    git_path(seg) || super::rest::is_web(seg) || matches!(seg, ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..] | ["api", "v3", ..] | ["api", "graphql"])
}

/// First segments that are kiln's own, never a repository owner.
pub fn reserved(owner: &str) -> bool {
    matches!(owner, "api" | "_kiln" | "_apis" | "oidc" | "twirp" | "assets" | "dl" | "runs" | "repos" | "agents" | "runners" | "deployments" | "settings" | "system")
}

/// git's smart HTTP for a repository: `/<owner>/<repo>[.git]/info/refs` and `…/git-upload-pack`.
fn git_path(seg: &[&str]) -> bool {
    match seg {
        [owner, _, "info", "refs"] | [owner, _, "git-upload-pack"] => !reserved(owner),
        _ => false,
    }
}

/// `/api/ci/<rest>`, authenticated.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], q: &HashMap<String, String>, req: Request) -> Result<Response, Fail> {
    let method = req.method().clone();
    match (&method, rest) {
        // W02: workflows.
        (&Method::POST, ["validate"]) => workflows::validate(req.into_body()).await,
        (&Method::GET, ["schema", kind]) => workflows::schema(kind),
        (&Method::GET, ["repos", repo, "workflows"]) => workflows::list(hub, repo, q).await,
        (&Method::GET, ["repos", repo, "workflows", path @ ..]) => workflows::get(hub, repo, &path.join("/"), q).await,
        // W09: secrets (names only) and variables.
        (_, ["secrets", ..] | ["variables", ..]) => super::secrets::route(hub, &method, rest, q, req.into_body()).await,
        // W03-R11: dispatches.
        (&Method::POST, ["repos", repo, "workflows", path @ .., "dispatches"]) => workflows::dispatch(hub, repo, &path.join("/"), req.into_body()).await,
        (&Method::POST, ["repos", repo, "dispatches"]) => workflows::repository_dispatch(hub, repo, req.into_body()).await,
        _ => Err((StatusCode::NOT_FOUND, format!("no such CI route: {method} /api/ci/{}", rest.join("/")))),
    }
}
