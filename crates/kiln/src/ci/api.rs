//! CI's HTTP surface (docs/ci.md §4, §2.6). Two entry points, called from `crate::api`:
//!
//! - [`open`]: routes that authenticate themselves before the console's auth applies: the GitHub
//!   runtime services (`/_kiln/…`, `/_apis/…`, W08), job ID tokens and the OIDC issuer's discovery
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
    let _ = (hub, &req);
    match seg {
        ["_kiln", ..] | ["_apis", ..] | ["oidc", ..] => Some(Err((StatusCode::NOT_FOUND, "not found".into()))),
        _ => None,
    }
}

/// Is this path one of [`open`]'s? (So the caller can hand the request over without cloning it.)
pub fn is_open(seg: &[&str]) -> bool {
    matches!(seg, ["_kiln", ..] | ["_apis", ..] | ["oidc", ..])
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
