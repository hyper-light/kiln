//! `heroku` (docs/cd-targets.md §3): a Heroku app, each release a build of the source CI packed,
//! and its config vars.
//!
//! ```yaml
//! web:
//!   heroku:
//!     app: shop-web                                   # made if it isn't there
//!     source: "${{ release.artifacts.source.digest }}"   # a .tar.gz of the source CI made
//!     config: { MODE: prod }                          # config vars kiln sets (others are left as they are)
//!     stack: heroku-24                                # for an app kiln makes
//! ```
//!
//! - **App** (`shop-web`): `spec` is what a release runs: `source` (`{artifact: sha256:…}`), and
//!   `config`, the config vars kiln sets. A release's source goes up to Heroku's sources and is
//!   built into a release; its dynos start on it. A rollback to a release kiln made before is
//!   Heroku's rollback to that release: its slug again, built from nothing.
//! - **Health** is the current release's and its dynos': up, starting, or crashed; and a newer
//!   build that failed.

use serde_json::{Map, Value, json};

use super::{RenderInput, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "heroku.kiln.dev";

/// The files a render reads: none, the source is CI's.
pub fn files(_spec: &Value) -> Vec<String> {
    Vec::new()
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let app = text(input.spec, "app").ok_or("heroku needs app: the app's name")?;
    if app.len() < 3 || app.len() > 30 || !app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || !app.as_bytes().first().is_some_and(u8::is_ascii_lowercase) {
        return Err(format!("app `{app}`: 3 to 30 lowercase letters, digits and -, starting with a letter"));
    }
    let source = text(input.spec, "source").ok_or("heroku needs source: a .tar.gz of the source CI made (${{ release.artifacts.<name>.digest }})")?;
    let hex = source.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("source `{source}`: a .tar.gz CI made (${{{{ release.artifacts.<name>.digest }}}})"))?;
    let mut spec = Map::new();
    spec.insert("source".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    if let Some(Value::Object(c)) = input.spec.get("config") {
        spec.insert("config".into(), Value::Object(c.iter().map(|(k, v)| (k.clone(), json!(v.as_str().map_or_else(|| v.to_string(), str::to_owned)))).collect()));
    }
    for k in ["stack", "region"] {
        if let Some(v) = text(input.spec, k) {
            spec.insert(k.into(), json!(v));
        }
    }
    Ok(vec![object(GROUP, "App", app, None, Value::Object(spec))])
}

/// An App object from Heroku's answers: the app, its current release, its config vars, its dynos
/// and its newest build. While the current release is the one kiln's last apply made (`runs`),
/// its source reads as kiln applied it; only the config vars kiln sets are read back.
pub fn live_app(app: &Value, release: Option<&Value>, config: &Value, dynos: &[Value], build: Option<&Value>, applied: Option<&Value>, runs: bool) -> Option<Value> {
    let name = app.get("name")?.as_str()?;
    let want = applied.and_then(|a| a.get("spec"));
    let mut spec = Map::new();
    if let (Some(s), true) = (want.and_then(|w| w.get("source")), runs) {
        spec.insert("source".into(), s.clone());
    }
    if let Some(keys) = want.and_then(|w| w.get("config")).and_then(Value::as_object) {
        spec.insert("config".into(), Value::Object(keys.keys().filter_map(|k| config.get(k).map(|v| (k.clone(), v.clone()))).collect()));
    }
    for k in ["stack", "region"] {
        if want.and_then(|w| w.get(k)).is_some()
            && let Some(v) = app.pointer(&format!("/{k}/name"))
        {
            spec.insert(k.into(), v.clone());
        }
    }
    let count = |s: &str| dynos.iter().filter(|d| d.get("state").and_then(Value::as_str) == Some(s)).count();
    let mut o = object(GROUP, "App", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert(
            "status".into(),
            json!({ "webUrl": app.get("web_url"),
                "release": release.map(|r| json!({ "id": r.get("id"), "version": r.get("version"), "status": r.get("status"), "description": r.get("description"), "createdAt": r.get("created_at") })),
                "build": build.map(|b| json!({ "id": b.get("id"), "status": b.get("status"), "createdAt": b.get("created_at") })),
                "dynos": { "up": count("up"), "starting": count("starting"), "crashed": count("crashed"), "total": dynos.len() } }),
        );
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "App" {
        return None;
    }
    let st = o.get("status")?;
    let n = |k: &str| st.pointer(&format!("/dynos/{k}")).and_then(Value::as_u64).unwrap_or(0);
    let at = |p: &str| st.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    // A build newer than the current release that failed: the release kiln asked for didn't come.
    if at("/build/status") == "failed" && at("/build/createdAt") > at("/release/createdAt") {
        return status(Health::Degraded, "its newest build failed");
    }
    match at("/release/status").as_str() {
        "failed" => return status(Health::Degraded, "its current release failed"),
        "pending" => return status(Health::Progressing, "its release is pending"),
        _ => {}
    }
    if n("crashed") > 0 {
        return status(Health::Degraded, format!("{} of its {} dynos crashed", n("crashed"), n("total")));
    }
    if n("starting") > 0 {
        return status(Health::Progressing, "its dynos are starting");
    }
    status(Health::Healthy, "")
}
