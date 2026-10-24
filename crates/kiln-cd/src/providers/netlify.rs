//! `netlify` (docs/cd-targets.md §3): a Netlify site, each release a deploy of the site CI built.
//!
//! ```yaml
//! site:
//!   netlify:
//!     site: shop-site                                 # made if it isn't there
//!     dir: "${{ release.artifacts.site.digest }}"     # the built site: a zip or .tar.gz CI made
//! ```
//!
//! - **Site** (`shop-site`): `spec.dir` is the built site (`{artifact: sha256:…}`). A deploy names
//!   each file by its SHA-1, and only the files Netlify doesn't have go up; it's published once
//!   ready. A rollback to a release kiln deployed before restores that deploy: nothing goes up.
//! - **Health** is the published deploy's and the newest one's: ready, processing, or the error.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "netlify.kiln.dev";

/// The files a render reads: none, the site is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let site = text(input.spec, "site").ok_or("netlify needs site: the site's name")?;
    if site.len() > 63 || !site.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(format!("site `{site}`: lowercase letters, digits and -"));
    }
    let dir = text(input.spec, "dir").ok_or("netlify needs dir: the built site, a zip or .tar.gz CI made (${{ release.artifacts.<name>.digest }})")?;
    let hex = dir.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("dir `{dir}`: a zip or .tar.gz CI made (${{{{ release.artifacts.<name>.digest }}}})"))?;
    let mut spec = Map::new();
    spec.insert("dir".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    Ok(vec![object(GROUP, "Site", site, None, Value::Object(spec))])
}

/// A Site object from Netlify's site and its newest deploy. While the published deploy is the
/// one kiln's last apply made (`runs`), it reads as kiln applied it.
pub fn live_site(site: &Value, newest: Option<&Value>, applied: Option<&Value>, runs: bool) -> Option<Value> {
    let name = site.get("name")?.as_str()?;
    let mut spec = Map::new();
    if let (Some(d), true) = (applied.and_then(|a| a.pointer("/spec/dir")), runs) {
        spec.insert("dir".into(), d.clone());
    }
    let mut o = object(GROUP, "Site", name, None, Value::Object(spec));
    let d = |v: Option<&Value>| v.map(|d| json!({ "id": d.get("id"), "state": d.get("state"), "error": d.get("error_message") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "id": site.get("id"), "url": site.get("ssl_url").or_else(|| site.get("url")), "published": d(site.get("published_deploy").filter(|p| !p.is_null())), "newest": d(newest) }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Site" {
        return None;
    }
    let st = o.get("status")?;
    let s = |p: &str| st.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    if s("/newest/state") == "error" && s("/newest/id") != s("/published/id") {
        return status(Health::Degraded, format!("its newest deploy failed: {}", s("/newest/error")));
    }
    match s("/published/state").as_str() {
        "ready" => status(Health::Healthy, ""),
        "" => status(Health::Progressing, "nothing published yet"),
        p => status(Health::Progressing, format!("its deploy is {p}")),
    }
}
