//! `azure-appservice` and `azure-functions` (docs/cd-targets.md §3): an App Service web app or a
//! function app (both `Microsoft.Web/sites`), each release its image or its package, deployed
//! through a slot and swapped in when the app names one.
//!
//! ```yaml
//! web:
//!   azure-appservice:                       # or azure-functions, for a function app
//!     app: shop-web                         # the app, made with its plan
//!     image: "acme.azurecr.io/web@${{ release.artifacts.image.digest }}"   # a container, or
//!     package: "${{ release.artifacts.site.digest }}"                     # a zip CI built
//!     slot: staging                         # deployed there, warmed, then swapped into production
//!     settings: { MODE: prod }              # app settings kiln sets (others are left as they are)
//!     config: { healthCheckPath: /healthz } # siteConfig fields
//! ```
//!
//! - **WebApp** / **FunctionApp** (`shop-web`): `spec` holds what kiln sets: `image` (the site's
//!   `linuxFxVersion`, `DOCKER|<image>`) or `package` (`{artifact: sha256:…}`, a zip deployed
//!   through the app's SCM site), `appSettings`, `siteConfig`, and `slot`. Only what kiln sets is
//!   read back: an app's other settings may hold secrets.
//! - **Rollouts.** Without a slot, a release deploys to the app and it restarts on it. With one,
//!   it deploys to the slot, which warms up, and the slot is swapped into production: the release
//!   before is then in the slot, so going back to it is one swap, with nothing deployed.
//! - **Health** is the site's: running and available, or the deployment that failed.

use serde_json::{Map, Value, json};

use super::gcp_run::valid_name;
use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const APPSERVICE: &str = "appservice.azure.kiln.dev";
pub const FUNCTIONS: &str = "functions.azure.kiln.dev";

/// Resource Manager's API version kiln speaks for sites.
pub const API: &str = "2023-12-01";

/// A provider's group and kind: `azure-appservice`'s WebApp, `azure-functions`' FunctionApp.
pub fn kind_of(provider: &str) -> (&'static str, &'static str) {
    if provider == "azure-functions" { (FUNCTIONS, "FunctionApp") } else { (APPSERVICE, "WebApp") }
}

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["config"])
}

/// What a release runs, by what kiln sets: the same marker, the same release.
pub fn marker(desired: &Value) -> String {
    format!("{:016x}", crate::obj::hash_value(desired.get("spec").unwrap_or(&Value::Null)))
}

/// An app's objects, for `provider`.
pub fn render(provider: &str, input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let (group, kind) = kind_of(provider);
    let app = text(input.spec, "app").ok_or_else(|| format!("{provider} needs app: the app's name"))?;
    if app.len() < 2 || app.len() > 60 || !app.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') || app.starts_with('-') || app.ends_with('-') {
        return Err(format!("app `{app}`: 2 to 60 letters, digits and -, not starting or ending with -"));
    }
    let mut spec = Map::new();
    match (text(input.spec, "image"), text(input.spec, "package")) {
        (Some(_), Some(_)) => return Err(format!("{provider} takes image (a container) or package (a zip), not both")),
        (Some(i), None) => {
            spec.insert("image".into(), json!(i));
        }
        (None, Some(p)) => {
            let hex = p.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(|| format!("package `{p}`: a zip CI built (${{{{ release.artifacts.<name>.digest }}}})"))?;
            spec.insert("package".into(), json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
        }
        (None, None) => return Err(format!("{provider} needs image (a container) or package (a zip CI built)")),
    }
    if let Some(Value::Object(s)) = input.spec.get("settings") {
        let settings: Map<String, Value> = s.iter().map(|(k, v)| (k.clone(), json!(v.as_str().map_or_else(|| v.to_string(), str::to_owned)))).collect();
        spec.insert("appSettings".into(), Value::Object(settings));
    }
    match file_or_object(input, "config")? {
        Some(c @ Value::Object(_)) => {
            spec.insert("siteConfig".into(), c);
        }
        Some(_) => return Err("config: siteConfig's fields, as a file or an object".into()),
        None => {}
    }
    if let Some(slot) = text(input.spec, "slot") {
        if !valid_name(slot, 59) || slot == "production" {
            return Err(format!("slot `{slot}`: a slot's name (lowercase letters, digits and -), not production"));
        }
        spec.insert("slot".into(), json!(slot));
    }
    let mut spec = Value::Object(spec);
    apply_sets(&mut spec, input.spec.get("set"))?;
    Ok(vec![object(group, kind, app, None, spec)])
}

/// A container image from a site's `linuxFxVersion` (`DOCKER|<image>`).
pub fn image_of(config: &Value) -> Option<String> {
    config.pointer("/properties/linuxFxVersion").and_then(Value::as_str).and_then(|v| v.strip_prefix("DOCKER|")).map(str::to_owned)
}

/// What a site reads as. `site`, `config` (`config/web`) and `settings` (its app settings) are
/// Azure's answers; `applied` what kiln last applied, and `runs` whether production still runs
/// it (the server's judgement: kiln's deployment is the newest, or its swap stands); `deployment`
/// the last deployment's record, when the site deploys packages.
pub fn live_site(provider: &str, site: &Value, config: &Value, settings: &Value, applied: Option<&Value>, runs: bool, deployment: Option<&Value>) -> Option<Value> {
    let (group, kind) = kind_of(provider);
    let name = site.get("name")?.as_str()?;
    let want = applied.and_then(|a| a.get("spec"));
    let mut spec = Map::new();
    if let Some(i) = image_of(config) {
        spec.insert("image".into(), json!(i));
    }
    if let (Some(p), true) = (want.and_then(|w| w.get("package")), runs) {
        spec.insert("package".into(), p.clone());
    }
    let live_settings = settings.get("properties").and_then(Value::as_object);
    if let (Some(keys), Some(have)) = (want.and_then(|w| w.get("appSettings")).and_then(Value::as_object), live_settings) {
        spec.insert("appSettings".into(), Value::Object(keys.keys().filter_map(|k| have.get(k).map(|v| (k.clone(), v.clone()))).collect()));
    }
    if let (Some(keys), Some(have)) = (want.and_then(|w| w.get("siteConfig")).and_then(Value::as_object), config.get("properties").and_then(Value::as_object)) {
        spec.insert("siteConfig".into(), Value::Object(keys.keys().filter_map(|k| have.get(k).map(|v| (k.clone(), v.clone()))).collect()));
    }
    if let Some(s) = want.and_then(|w| w.get("slot")) {
        spec.insert("slot".into(), s.clone());
    }
    let mut o = object(group, kind, name, None, Value::Object(spec));
    let p = |k: &str| site.pointer(&format!("/properties/{k}")).cloned().unwrap_or(Value::Null);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "state": p("state"), "availabilityState": p("availabilityState"), "defaultHostName": p("defaultHostName"), "deployment": deployment }));
    }
    Some(o)
}

/// The SCM site's host an app (or slot) deploys packages through.
pub fn scm_host(site: &Value) -> Option<String> {
    let states = site.pointer("/properties/hostNameSslStates").and_then(Value::as_array);
    states
        .into_iter()
        .flatten()
        .find(|h| h.get("hostType").and_then(Value::as_str) == Some("Repository"))
        .and_then(|h| h.get("name").and_then(Value::as_str))
        .or_else(|| site.pointer("/properties/enabledHostNames").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).find(|h| h.contains(".scm.")))
        .map(str::to_owned)
}

pub fn health(o: &Value) -> Option<Status> {
    if !matches!(crate::obj::kind(o), "WebApp" | "FunctionApp") {
        return None;
    }
    let st = o.get("status")?;
    if st.pointer("/deployment/status").and_then(Value::as_i64) == Some(3) {
        let why = st.pointer("/deployment/status_text").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("its deployment failed");
        return status(Health::Degraded, why);
    }
    match (st.get("state").and_then(Value::as_str).unwrap_or_default(), st.get("availabilityState").and_then(Value::as_str).unwrap_or("Normal")) {
        ("Running", "Normal") => status(Health::Healthy, ""),
        ("Running", a) => status(Health::Degraded, format!("the site's availability is {a}")),
        ("Stopped", _) => status(Health::Suspended, "stopped"),
        (s, _) => status(Health::Progressing, format!("the site is {}", if s.is_empty() { "starting" } else { s })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_render_and_read_back_only_what_kiln_sets() -> Result<(), String> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let read = |_: &str| None;
        let hex = "ef".repeat(32);
        let spec = json!({ "app": "shop-web", "package": format!("sha256:{hex}"), "slot": "staging", "settings": { "MODE": "prod", "WORKERS": 4 }, "config": { "healthCheckPath": "/healthz" } });
        let objs = render("azure-functions", &RenderInput { spec: &spec, target: &EMPTY, app: "default/shop-staging", namespace: "", revision: "c0ffee", read: &read })?;
        let o = objs.first().ok_or("no app")?;
        assert_eq!((crate::obj::group(o), crate::obj::kind(o)), (FUNCTIONS, "FunctionApp"));
        assert_eq!(o.get("spec"), Some(&json!({ "package": { "artifact": format!("sha256:{hex}") }, "slot": "staging", "appSettings": { "MODE": "prod", "WORKERS": "4" }, "siteConfig": { "healthCheckPath": "/healthz" } })));
        let site = json!({ "name": "shop-web", "properties": { "state": "Running", "availabilityState": "Normal" } });
        let config = json!({ "properties": { "healthCheckPath": "/healthz", "alwaysOn": true } });
        let settings = json!({ "properties": { "MODE": "prod", "WORKERS": "4", "DB_PASSWORD": "hunter2" } });
        let live = live_site("azure-functions", &site, &config, &settings, Some(o), true, None).ok_or("no live")?;
        assert_eq!(live.get("spec"), o.get("spec"), "{live}");
        assert!(!live.to_string().contains("hunter2"), "another's setting isn't read back");
        assert_eq!(health(&live).map(|s| s.health), Some(Health::Healthy));
        let gone = live_site("azure-functions", &site, &config, &settings, Some(o), false, None).ok_or("no live")?;
        assert_eq!(gone.pointer("/spec/package"), None, "a package that no longer runs isn't read as kiln's");
        assert!(render("azure-appservice", &RenderInput { spec: &json!({ "app": "x1", "image": "a", "package": format!("sha256:{hex}") }), target: &EMPTY, app: "a", namespace: "", revision: "c", read: &read }).is_err());
        Ok(())
    }
}
