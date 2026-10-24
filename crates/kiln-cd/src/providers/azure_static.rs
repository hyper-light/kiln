//! `azure-static` (docs/cd-targets.md §3): a static web app, each release's built site deployed
//! to it.
//!
//! ```yaml
//! site:
//!   azure-static:
//!     app: shop-site
//!     site: "${{ release.artifacts.site.digest }}"   # the built site: a zip CI made
//!     sku: Standard                                  # for an app kiln makes (Free by default)
//!     storage: acmeartifacts                         # where the zips wait (kiln's own account by default)
//! ```
//!
//! - **StaticSite** (`shop-site`): `spec.site` is the zip (`{artifact: sha256:…}`). Static Web
//!   Apps fetches a deployment from a URL, so kiln keeps each zip in a storage account (kiln's own
//!   in the resource group, made the first time, or `storage:`) and gives Azure a URL signed to
//!   read that one blob for an hour. A rollback deploys the earlier release's zip, which is there
//!   still: nothing is uploaded.
//! - **Health** is the production build's: ready, deploying, or failed.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "staticwebapps.azure.kiln.dev";

/// Resource Manager's API version kiln speaks for static web apps.
pub const API: &str = "2023-12-01";

/// The files a render reads: none, the site is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let app = text(input.spec, "app").ok_or("azure-static needs app: the static web app's name")?;
    if app.len() < 2 || app.len() > 40 || !app.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!("app `{app}`: 2 to 40 letters, digits and -"));
    }
    let site = text(input.spec, "site").ok_or("azure-static needs site: the built site, a zip CI made (${{ release.artifacts.<name>.digest }})")?;
    let hex = site.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("site `{site}`: a zip CI made (${{{{ release.artifacts.<name>.digest }}}})"))?;
    let mut spec = Map::new();
    spec.insert("site".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    if let Some(sku) = text(input.spec, "sku") {
        if !matches!(sku, "Free" | "Standard") {
            return Err(format!("sku `{sku}`: Free or Standard"));
        }
        spec.insert("sku".into(), json!({ "name": sku, "tier": sku }));
    }
    if let Some(s) = text(input.spec, "storage") {
        spec.insert("storage".into(), json!(s));
    }
    Ok(vec![object(GROUP, "StaticSite", app, None, Value::Object(spec))])
}

/// A StaticSite object from Azure's answers: the app and its production build. `applied` is what
/// kiln last applied, and `runs` whether production still serves its deployment (the server's
/// judgement, from the build kiln's deployment made).
pub fn live_site(answer: &Value, build: Option<&Value>, applied: Option<&Value>, runs: bool) -> Option<Value> {
    let name = answer.get("name")?.as_str()?;
    let want = applied.and_then(|a| a.get("spec"));
    let mut spec = Map::new();
    if let (Some(s), true) = (want.and_then(|w| w.get("site")), runs) {
        spec.insert("site".into(), s.clone());
    }
    if want.and_then(|w| w.get("sku")).is_some()
        && let Some(sku) = answer.get("sku")
    {
        spec.insert("sku".into(), json!({ "name": sku.get("name"), "tier": sku.get("tier") }));
    }
    if let Some(s) = want.and_then(|w| w.get("storage")) {
        spec.insert("storage".into(), s.clone());
    }
    let mut o = object(GROUP, "StaticSite", name, None, Value::Object(spec));
    let b = |k: &str| build.and_then(|b| b.pointer(&format!("/properties/{k}"))).cloned().unwrap_or(Value::Null);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "defaultHostname": answer.pointer("/properties/defaultHostname"), "build": { "status": b("status"), "lastUpdatedOn": b("lastUpdatedOn") } }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "StaticSite" {
        return None;
    }
    match o.pointer("/status/build/status").and_then(Value::as_str).unwrap_or_default() {
        "Ready" => status(Health::Healthy, ""),
        "Failed" => status(Health::Degraded, "its deployment failed"),
        "" => status(Health::Progressing, "nothing deployed yet"),
        s => status(Health::Progressing, format!("its deployment is {s}")),
    }
}
