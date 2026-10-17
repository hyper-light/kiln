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
        ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..] => Some(Err((StatusCode::NOT_FOUND, "not found".into()))),
        _ => None,
    }
}

/// Is this path one of [`open`]'s? (So the caller can hand the request over without cloning it.)
pub fn is_open(seg: &[&str]) -> bool {
    git_path(seg) || matches!(seg, ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] | ["twirp", ..])
}

/// git's smart HTTP for a repository: `/<owner>/<repo>[.git]/info/refs` and `…/git-upload-pack`.
fn git_path(seg: &[&str]) -> bool {
    match seg {
        [owner, _, "info", "refs"] | [owner, _, "git-upload-pack"] => !matches!(*owner, "api" | "_kiln" | "_apis" | "oidc" | "twirp" | "assets" | "dl"),
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
        _ => Err((StatusCode::NOT_FOUND, format!("no such CI route: {method} /api/ci/{}", rest.join("/")))),
    }
}
