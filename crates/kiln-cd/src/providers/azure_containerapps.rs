//! `azure-containerapps` (docs/cd-targets.md §3): a container app, each release's template a
//! revision of its own that takes the traffic once it runs.
//!
//! ```yaml
//! shop:
//!   azure-containerapps:
//!     app: shop
//!     environment: shop-env                    # the managed environment: a name here, or its id
//!     image: "acme.azurecr.io/shop@${{ release.artifacts.image.digest }}"
//!     spec: deploy/containerapp.yaml           # the app's YAML (az containerapp show -o yaml), or an object
//!     set: { "properties.template.scale.maxReplicas": 4 }
//! ```
//!
//! - **ContainerApp** (`shop`): `spec` is the app as Resource Manager spells it (`location`,
//!   `tags`, `properties.configuration`, `properties.template`). `image` is the first
//!   container's. kiln names the template's revision for its content (`revisionSuffix` `k<hash>`,
//!   so `shop--k<hash>`) and runs the app in `Multiple` revision mode, so revisions stay to go
//!   back to. With ingress, the release's revision gets all the traffic; without it, it's the one
//!   kiln keeps active. A rollback moves the traffic back to its revision, activated again if it
//!   was let go: nothing is made again.
//! - **Health** is the serving revision's: provisioned and running, or failed with its message.

use serde_json::{Map, Value, json};

use super::gcp_run::{covers, valid_name};
use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "app.azure.kiln.dev";

/// Resource Manager's API version kiln speaks for container apps.
pub const API: &str = "2024-03-01";

/// An app's fields Azure sets, which kiln never does: at the top, and in its properties.
const OUTPUT: &[&str] = &["id", "name", "type", "systemData", "etag"];
const OUTPUT_PROPS: &[&str] = &[
    "provisioningState",
    "runningStatus",
    "latestRevisionName",
    "latestReadyRevisionName",
    "latestRevisionFqdn",
    "customDomainVerificationId",
    "outboundIpAddresses",
    "eventStreamEndpoint",
    "managedEnvironmentId",
];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["spec"])
}

/// A template's revision suffix, by its content: `k<hash>`.
pub fn suffix(template: &Value) -> String {
    let mut t = template.clone();
    if let Some(m) = t.as_object_mut() {
        m.remove("revisionSuffix");
    }
    format!("k{}", &format!("{:016x}", crate::obj::hash_value(&t))[..10])
}

/// A revision's name: `<app>--<suffix>`.
pub fn revision_name(app: &str, suffix: &str) -> String {
    format!("{app}--{suffix}")
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let app = text(input.spec, "app").ok_or("azure-containerapps needs app: the container app's name")?;
    if !valid_name(app, 32) || app.contains("--") {
        return Err(format!("app `{app}`: 2 to 32 lowercase letters, digits and -, starting with a letter, without --"));
    }
    let mut res = match file_or_object(input, "spec")? {
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err("spec: the app's YAML, as a file or an object".into()),
        None => json!({}),
    };
    // The YAML's properties may stand at its top.
    if res.get("properties").is_none() && (res.get("template").is_some() || res.get("configuration").is_some()) {
        res = json!({ "properties": res });
    }
    let m = res.as_object_mut().ok_or("spec: an object")?;
    for k in OUTPUT {
        m.remove(*k);
    }
    let props = crate::obj::ensure_map(&mut res, &["properties"]).ok_or("spec.properties: an object")?;
    for k in OUTPUT_PROPS {
        props.remove(*k);
    }
    if let Some(e) = text(input.spec, "environment") {
        props.insert("environmentId".into(), json!(e));
    }
    if let Some(image) = text(input.spec, "image") {
        let template = props.entry("template").or_insert_with(|| json!({}));
        let containers = crate::obj::ensure_map(template, &[]).ok_or("properties.template: an object")?.entry("containers").or_insert_with(|| json!([]));
        let Value::Array(list) = containers else { return Err("properties.template.containers: a list".into()) };
        if list.is_empty() {
            list.push(json!({ "name": app }));
        }
        match list.first_mut() {
            Some(Value::Object(c)) => {
                c.insert("image".into(), json!(image));
            }
            _ => return Err("properties.template.containers[0]: an object".into()),
        }
    }
    apply_sets(&mut res, input.spec.get("set"))?;
    let props = crate::obj::ensure_map(&mut res, &["properties"]).ok_or("spec.properties: an object")?;
    if !props.get("environmentId").and_then(Value::as_str).is_some_and(|e| !e.is_empty()) {
        return Err("azure-containerapps needs environment: the managed environment the app runs in".into());
    }
    let has_image = props.get("template").and_then(|t| t.get("containers")).and_then(|c| c.get(0)).and_then(|c| c.get("image")).and_then(Value::as_str).is_some_and(|i| !i.is_empty());
    if !has_image {
        return Err("azure-containerapps needs image: the image to run, or the template's first container's in spec".into());
    }
    let config = crate::obj::ensure_map(&mut res, &["properties", "configuration"]).ok_or("properties.configuration: an object")?;
    config.insert("activeRevisionsMode".into(), json!("Multiple"));
    let template = crate::obj::ensure_map(&mut res, &["properties", "template"]).ok_or("properties.template: an object")?;
    template.remove("revisionSuffix");
    let sfx = suffix(&Value::Object(template.clone()));
    template.insert("revisionSuffix".into(), json!(sfx));
    if let Some(ingress) = res.pointer_mut("/properties/configuration/ingress").and_then(Value::as_object_mut) {
        ingress.remove("fqdn");
        ingress.insert("traffic".into(), json!([{ "revisionName": revision_name(app, &sfx), "weight": 100 }]));
    }
    Ok(vec![object(GROUP, "ContainerApp", app, None, res)])
}

// ------------------------------------------------------------------ Azure's answers → objects

/// The revision an app serves, as configured: the one with all the ingress traffic (the latest
/// ready one's, for `latestRevision`), or, without ingress, `active` (the one kiln keeps active).
pub fn serving(o: &Value, active: Option<&str>) -> Option<String> {
    let Some(traffic) = o.pointer("/spec/properties/configuration/ingress/traffic").and_then(Value::as_array) else { return active.map(str::to_owned) };
    let taking: Vec<&Value> = traffic.iter().filter(|t| t.get("weight").and_then(Value::as_u64).unwrap_or(0) > 0).collect();
    let [t] = taking.as_slice() else { return None };
    if t.get("weight").and_then(Value::as_u64) != Some(100) {
        return None;
    }
    if t.get("latestRevision").and_then(Value::as_bool) == Some(true) {
        return o.pointer("/status/latestReadyRevisionName").and_then(Value::as_str).map(str::to_owned);
    }
    t.get("revisionName").and_then(Value::as_str).map(str::to_owned)
}

/// A ContainerApp object from Azure's answer. `environment`: what kiln applied for its
/// environment and the id that resolves to; `revision`: the serving revision's state, and
/// `active` the revision kiln keeps active without ingress (both read by the server).
pub fn live_app(answer: &Value, applied: Option<&Value>, environment: Option<(&str, &str)>, revision: Option<&Value>, active: Option<&str>) -> Option<Value> {
    let name = answer.get("name")?.as_str()?;
    let mut spec = answer.as_object()?.clone();
    for k in OUTPUT {
        spec.remove(*k);
    }
    let app = spec.get_mut("tags").and_then(Value::as_object_mut).and_then(|t| t.remove("kiln-app"));
    let tracking = spec.get_mut("tags").and_then(Value::as_object_mut).and_then(|t| t.remove("kiln-tracking"));
    if spec.get("tags").and_then(Value::as_object).is_some_and(Map::is_empty) {
        spec.remove("tags");
    }
    let (state, latest, ready, fqdn) = {
        let p = answer.get("properties");
        let s = |k: &str| p.and_then(|p| p.get(k)).cloned().unwrap_or(Value::Null);
        (s("provisioningState"), s("latestRevisionName"), s("latestReadyRevisionName"), answer.pointer("/properties/configuration/ingress/fqdn").cloned().unwrap_or(Value::Null))
    };
    if let Some(props) = spec.get_mut("properties").and_then(Value::as_object_mut) {
        for k in OUTPUT_PROPS {
            props.remove(*k);
        }
        if let (Some((given, id)), Some(have)) = (environment, props.get("environmentId").and_then(Value::as_str))
            && have.eq_ignore_ascii_case(id)
        {
            props.insert("environmentId".into(), json!(given));
        }
        if let Some(i) = props.get_mut("configuration").and_then(|c| c.get_mut("ingress")).and_then(Value::as_object_mut) {
            i.remove("fqdn");
        }
    }
    let mut o = object(GROUP, "ContainerApp", name, None, Value::Object(spec));
    if let Some(Value::String(a)) = app
        && let Some(l) = crate::obj::ensure_map(&mut o, &["metadata", "labels"])
    {
        l.insert(crate::engine::APP_LABEL.into(), json!(a));
    }
    if let Some(Value::String(t)) = tracking
        && let Some(a) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"])
    {
        a.insert(crate::engine::TRACKING.into(), json!(t));
    }
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "provisioningState": state, "latestRevisionName": latest, "latestReadyRevisionName": ready, "fqdn": fqdn, "revision": revision, "active": active }));
    }
    // While the release's revision serves, the template reads as kiln applied it: Azure keeps the
    // newest template it was given, which after a rollback is a later release's.
    if let Some(t) = applied.and_then(|a| a.pointer("/spec/properties/template"))
        && let Some(sfx) = t.get("revisionSuffix").and_then(Value::as_str)
        && serving(&o, active).as_deref() == Some(revision_name(name, sfx).as_str())
        && let Some(props) = o.pointer_mut("/spec/properties").and_then(Value::as_object_mut)
    {
        props.insert("template".into(), t.clone());
    }
    Some(o)
}

/// The app kiln sends (a PUT): what it is now (`live`, Azure's answer) with what `desired` sets
/// over it, its environment resolved to `environment_id`, the template as it is when
/// `keep_template` (the revision kiln wants is there already), and kiln's tags.
pub fn app_body(live: Option<&Value>, desired: &Value, environment_id: &str, keep_template: bool, tags: Map<String, Value>) -> Value {
    let mut body = live.cloned().unwrap_or_else(|| json!({}));
    if let Some(m) = body.as_object_mut() {
        for k in OUTPUT {
            m.remove(*k);
        }
    }
    let live_template = live.and_then(|l| l.pointer("/properties/template")).cloned();
    let mut want = desired.get("spec").cloned().unwrap_or_else(|| json!({}));
    if keep_template
        && let (Some(t), Some(props)) = (live_template, want.get_mut("properties").and_then(Value::as_object_mut))
    {
        props.insert("template".into(), t);
    }
    body = super::gcp_functions::merged(&body, &want);
    if let Some(props) = body.get_mut("properties").and_then(Value::as_object_mut) {
        for k in OUTPUT_PROPS {
            props.remove(*k);
        }
        props.insert("environmentId".into(), json!(environment_id));
        // The template and traffic are as kiln says, not merged into what was.
        if let Some(t) = want.pointer("/properties/template") {
            props.insert("template".into(), t.clone());
        }
        if let (Some(tr), Some(i)) = (want.pointer("/properties/configuration/ingress/traffic"), props.get_mut("configuration").and_then(|c| c.get_mut("ingress")).and_then(Value::as_object_mut)) {
            i.insert("traffic".into(), tr.clone());
        }
    }
    if let Some(t) = crate::obj::ensure_map(&mut body, &["tags"]) {
        t.extend(tags);
    }
    body
}

/// Whether a PUT of `body` would change nothing of `live`.
pub fn unchanged(live: &Value, body: &Value) -> bool {
    let traffic = |v: &Value| v.pointer("/properties/configuration/ingress/traffic").cloned();
    traffic(live) == traffic(body) && covers(live, body)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "ContainerApp" {
        return None;
    }
    let st = o.get("status")?;
    match st.get("provisioningState").and_then(Value::as_str).unwrap_or_default() {
        "Failed" | "Canceled" => return status(Health::Degraded, "Azure couldn't provision the app"),
        "Succeeded" => {}
        _ => return status(Health::Progressing, "Azure is provisioning the app"),
    }
    let Some(r) = st.get("revision").filter(|r| !r.is_null()) else { return status(Health::Progressing, "no revision serves yet") };
    let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default();
    let why = format!("revision {}: {}", s("name"), if s("message").is_empty() { s("runningState") } else { s("message") });
    let failed = matches!((s("provisioningState"), s("runningState"), s("healthState")), ("Failed", _, _) | (_, "Failed" | "Degraded", _) | (_, _, "Unhealthy"));
    if failed {
        return status(Health::Degraded, why);
    }
    if r.get("active").and_then(Value::as_bool) == Some(false) {
        return status(Health::Suspended, format!("revision {} isn't active", s("name")));
    }
    match (s("provisioningState"), s("runningState"), s("healthState")) {
        ("Provisioned", "Running" | "RunningAtMaxScale" | "", "Healthy" | "None" | "") => status(Health::Healthy, ""),
        _ => status(Health::Progressing, why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_named_for_templates() -> Result<(), String> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let yaml = "properties:\n  configuration:\n    ingress: { external: true, targetPort: 8080, fqdn: shop.example.io }\n  template:\n    scale: { minReplicas: 1, maxReplicas: 3 }\n    containers: [{ name: web, resources: { cpu: 0.5, memory: 1Gi } }]\nlocation: westeurope\n";
        let read = |p: &str| (p == "app.yaml").then(|| yaml.as_bytes().to_vec());
        let spec = |image: &str| json!({ "app": "shop", "environment": "shop-env", "image": image, "spec": "app.yaml" });
        let render_of = |s: &Value| render(&RenderInput { spec: s, target: &EMPTY, app: "default/shop-staging", namespace: "", revision: "c0ffee", read: &read });
        let a = render_of(&spec("acme.azurecr.io/shop@sha256:aa"))?;
        let o = a.first().ok_or("no app")?;
        let sfx = o.pointer("/spec/properties/template/revisionSuffix").and_then(Value::as_str).ok_or("no suffix")?;
        assert_eq!(o.pointer("/spec/properties/configuration/activeRevisionsMode"), Some(&json!("Multiple")));
        assert_eq!(o.pointer("/spec/properties/configuration/ingress/traffic"), Some(&json!([{ "revisionName": format!("shop--{sfx}"), "weight": 100 }])));
        assert_eq!(o.pointer("/spec/properties/configuration/ingress/fqdn"), None, "Azure's, not kiln's");
        assert_eq!(o.pointer("/spec/properties/template/containers/0"), Some(&json!({ "name": "web", "resources": { "cpu": 0.5, "memory": "1Gi" }, "image": "acme.azurecr.io/shop@sha256:aa" })));
        assert_eq!(o.pointer("/spec/properties/environmentId"), Some(&json!("shop-env")));
        let b = render_of(&spec("acme.azurecr.io/shop@sha256:bb"))?;
        assert_ne!(b.first().and_then(|o| o.pointer("/spec/properties/template/revisionSuffix")), Some(&json!(sfx)));
        assert!(render_of(&json!({ "app": "shop", "image": "x" })).is_err_and(|e| e.contains("environment")));
        Ok(())
    }
}
