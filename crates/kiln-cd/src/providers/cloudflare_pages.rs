//! `cloudflare-pages` (docs/cd-targets.md §3): a Pages project, each release a direct upload of
//! the site CI built, deployed to production.
//!
//! ```yaml
//! site:
//!   cloudflare-pages:
//!     project: shop-site                              # made if it isn't there
//!     site: "${{ release.artifacts.site.digest }}"    # the built site: a zip or .tar.gz CI made
//!     branch: main                                    # the production branch (main)
//! ```
//!
//! - **Project** (`shop-site`): `spec.site` is the built site (`{artifact: sha256:…}`). Its files
//!   go up as wrangler's direct upload sends them, by their hashes, only those Pages lacks; a
//!   deployment of them to the production branch is waited for until deployed. A rollback to a
//!   release kiln deployed before is Pages' rollback to that deployment: nothing goes up.
//! - **Health** is the production deployment's, and a newer one's that failed.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "pages.cloudflare.kiln.dev";

/// The files a render reads: none, the site is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let project = text(input.spec, "project").ok_or("cloudflare-pages needs project: the Pages project")?;
    if project.len() > 58 || !project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || project.starts_with('-') {
        return Err(format!("project `{project}`: up to 58 lowercase letters, digits and -"));
    }
    let site = text(input.spec, "site").ok_or("cloudflare-pages needs site: the built site, a zip or .tar.gz CI made (${{ release.artifacts.<name>.digest }})")?;
    let hex = site.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("site `{site}`: a zip or .tar.gz CI made (${{{{ release.artifacts.<name>.digest }}}})"))?;
    let mut spec = Map::new();
    spec.insert("site".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    spec.insert("productionBranch".into(), json!(text(input.spec, "branch").unwrap_or("main")));
    Ok(vec![object(GROUP, "Project", project, None, Value::Object(spec))])
}

fn deployment(d: Option<&Value>) -> Value {
    d.filter(|d| !d.is_null()).map_or(Value::Null, |d| json!({ "id": d.get("id"), "stage": d.pointer("/latest_stage/name"), "status": d.pointer("/latest_stage/status"), "url": d.get("url") }))
}

/// A Project object from Pages' answer. While production serves the deployment kiln's last apply
/// made (`runs`), its site reads as kiln applied it.
pub fn live_project(project: &Value, applied: Option<&Value>, runs: bool) -> Option<Value> {
    let name = project.get("name")?.as_str()?;
    let mut spec = Map::new();
    if let (Some(s), true) = (applied.and_then(|a| a.pointer("/spec/site")), runs) {
        spec.insert("site".into(), s.clone());
    }
    if let Some(b) = project.get("production_branch") {
        spec.insert("productionBranch".into(), b.clone());
    }
    let mut o = object(GROUP, "Project", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "subdomain": project.get("subdomain"), "canonical": deployment(project.get("canonical_deployment")), "latest": deployment(project.get("latest_deployment")) }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Project" {
        return None;
    }
    let st = o.get("status")?;
    let s = |p: &str| st.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    if s("/latest/status") == "failure" && s("/latest/id") != s("/canonical/id") {
        return status(Health::Degraded, format!("its newest deployment failed at {}", s("/latest/stage")));
    }
    match (s("/canonical/stage").as_str(), s("/canonical/status").as_str()) {
        ("deploy", "success") => status(Health::Healthy, ""),
        ("", _) => status(Health::Progressing, "nothing deployed yet"),
        (stage, st) => status(Health::Progressing, format!("its deployment is at {stage}: {st}")),
    }
}
