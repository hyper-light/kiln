//! `render` (docs/cd-targets.md §3): a Render service, each release a deploy of its image (or
//! commit).
//!
//! ```yaml
//! web:
//!   render:
//!     service: shop-web                     # the service, made on Render
//!     image: "docker.io/acme/web@${{ release.artifacts.image.digest }}"   # an image-backed service's
//!     # commit: "${{ release.version }}"   # or a git-backed service's commit
//! ```
//!
//! - **Service** (`shop-web`): `spec` is what each release deploys: `image` (an image-backed
//!   service's, as Render's `imageUrl`) or `commit` (a git-backed one's). Render builds and rolls
//!   the deploy out, the one before serving until it's live. A rollback to a release kiln deployed
//!   before uses Render's rollback to that deploy: nothing is built again.
//! - **Health** is the newest deploy's: live, in progress, or failed with its status.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "render.kiln.dev";

/// The files a render reads: none.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let service = text(input.spec, "service").ok_or("render needs service: the Render service")?;
    let mut spec = Map::new();
    match (text(input.spec, "image"), text(input.spec, "commit")) {
        (Some(_), Some(_)) => return Err("render deploys an image or a commit, not both".into()),
        (Some(i), None) => {
            spec.insert("image".into(), json!(i));
        }
        (None, Some(c)) => {
            spec.insert("commit".into(), json!(c));
        }
        (None, None) => return Err("render needs image (an image-backed service's) or commit (a git-backed one's)".into()),
    }
    Ok(vec![object(GROUP, "Service", service, None, Value::Object(spec))])
}

/// What a deploy ran: its image or commit, in `spec`'s terms.
pub fn deployed(deploy: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(i) = deploy.pointer("/image/ref").and_then(Value::as_str) {
        m.insert("image".into(), json!(i));
    } else if let Some(c) = deploy.pointer("/commit/id").and_then(Value::as_str) {
        m.insert("commit".into(), json!(c));
    }
    m
}

/// A Service object from Render's service and its deploys (newest first). While the deploy
/// serving is one kiln made (`ours`: its id, and what kiln applied), it reads as kiln applied it.
pub fn live_service(service: &Value, deploys: &[Value], ours: Option<(&str, &Value)>) -> Option<Value> {
    let name = service.get("name")?.as_str()?;
    let serving = deploys.iter().find(|d| d.get("status").and_then(Value::as_str) == Some("live"));
    let spec = match (serving, ours) {
        (Some(d), Some((id, applied))) if d.get("id").and_then(Value::as_str) == Some(id) => applied.get("spec").cloned().unwrap_or(Value::Null),
        (Some(d), _) => Value::Object(deployed(d)),
        (None, _) => json!({}),
    };
    let mut o = object(GROUP, "Service", name, None, spec);
    let newest = deploys.first().map(|d| json!({ "id": d.get("id"), "status": d.get("status"), "trigger": d.get("trigger") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "id": service.get("id"), "url": service.pointer("/serviceDetails/url"), "suspended": service.get("suspended"), "serving": serving.and_then(|d| d.get("id")), "deploy": newest }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Service" {
        return None;
    }
    let st = o.get("status")?;
    if st.get("suspended").and_then(Value::as_str) == Some("suspended") {
        return status(Health::Suspended, "suspended");
    }
    let s = st.pointer("/deploy/status").and_then(Value::as_str).unwrap_or_default();
    match s {
        "live" => status(Health::Healthy, ""),
        "build_failed" | "update_failed" | "pre_deploy_failed" | "canceled" => status(Health::Degraded, format!("its newest deploy: {s}")),
        "" => status(Health::Progressing, "nothing deployed yet"),
        _ => status(Health::Progressing, format!("deploying: {s}")),
    }
}
