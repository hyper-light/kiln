//! `gcp-compute` (docs/cd-targets.md §3): a managed instance group, rolled onto an instance
//! template of each release's.
//!
//! ```yaml
//! web:
//!   gcp-compute:
//!     group: web
//!     template: web                       # the templates' name: kiln makes web-<hash> for each release
//!     image: "projects/acme/global/images/web-${{ release.version }}"
//!     properties: deploy/instance.json    # InstanceProperties: machineType, disks, networkInterfaces, metadata, …
//!     set: { "machineType": "e2-standard-2" }
//!     group-spec: { targetSize: 3, namedPorts: [{ name: http, port: 8080 }] }
//!     update: { maxSurge: { fixed: 1 }, maxUnavailable: { fixed: 0 } }
//! ```
//!
//! - **InstanceTemplate** (`web`, wave 0): instance templates never change once made, so kiln
//!   makes one for each release's properties, named for them (`web-<hash>`), and keeps those it
//!   made: a rollback finds its template there. The object reads as the template kiln made last.
//! - **InstanceGroupManager** (`web`, wave 1): the group, its one version on the release's
//!   template, with an update policy that rolls it out (`PROACTIVE`, `REPLACE` and the `update:`
//!   limits): Compute Engine replaces the instances, within the surge and unavailable limits.
//!   Health is the group's: stable and on its version, or the errors its instances meet.
//!
//! References to other resources (a health check, a target pool) are read back as
//! `projects/<project>/…`, without Compute Engine's URL prefix.

use serde_json::{Map, Value, json};

use super::gcp_run::{short, valid_name};
use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "compute.gcp.kiln.dev";

/// Compute Engine's URL prefixes, which references are read back without.
const PREFIXES: &[&str] = &["https://www.googleapis.com/compute/v1/", "https://compute.googleapis.com/compute/v1/"];

/// A group's fields Compute Engine sets, which kiln never does.
const GROUP_OUTPUT: &[&str] = &[
    "id",
    "kind",
    "name",
    "zone",
    "region",
    "selfLink",
    "creationTimestamp",
    "fingerprint",
    "status",
    "currentActions",
    "instanceGroup",
    "instanceTemplate",
    "satisfiesPzi",
    "satisfiesPzs",
    "errors",
];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["properties", "group-spec"])
}

/// References without Compute Engine's URL prefix, all the way down.
pub fn unprefixed(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(PREFIXES.iter().find_map(|p| s.strip_prefix(p)).unwrap_or(s).to_owned()),
        Value::Array(a) => Value::Array(a.iter().map(unprefixed).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), unprefixed(x))).collect()),
        other => other.clone(),
    }
}

/// The template kiln makes for a release's template spec: `<template>-<hash>`.
pub fn template_name(base: &str, spec: &Value) -> String {
    format!("{base}-{}", &format!("{:016x}", crate::obj::hash_value(spec))[..10])
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let group = text(input.spec, "group").ok_or("gcp-compute needs group: the managed instance group")?;
    let base = text(input.spec, "template").ok_or("gcp-compute needs template: the name kiln makes the group's instance templates under")?;
    if !valid_name(group, 63) {
        return Err(format!("group `{group}`: up to 63 lowercase letters, digits and -, starting with a letter"));
    }
    if !valid_name(base, 52) {
        return Err(format!("template `{base}`: up to 52 lowercase letters, digits and -, starting with a letter (kiln adds -<hash>)"));
    }
    let mut properties = match file_or_object(input, "properties")? {
        Some(v @ Value::Object(_)) => unprefixed(&v),
        Some(_) => return Err("properties: InstanceProperties, as a file or an object".into()),
        None => json!({}),
    };
    if let Some(image) = text(input.spec, "image") {
        let disks = crate::obj::ensure_map(&mut properties, &[]).ok_or("properties: an object")?.entry("disks").or_insert_with(|| json!([]));
        let Value::Array(list) = disks else { return Err("properties.disks: a list".into()) };
        if list.is_empty() {
            list.push(json!({ "boot": true, "autoDelete": true, "initializeParams": {} }));
        }
        let boot = list.iter_mut().find(|d| d.get("boot").and_then(Value::as_bool) == Some(true)).ok_or("properties.disks: none is the boot disk")?;
        let params = crate::obj::ensure_map(boot, &["initializeParams"]).ok_or("properties.disks[boot].initializeParams: an object")?;
        params.insert("sourceImage".into(), json!(image));
    }
    apply_sets(&mut properties, input.spec.get("set"))?;
    if !properties.get("machineType").and_then(Value::as_str).is_some_and(|m| !m.is_empty()) {
        return Err("gcp-compute needs the template's machineType (properties, or set: { machineType: e2-medium })".into());
    }
    let template = json!({ "properties": properties });
    let made = template_name(base, &template);
    let mut spec = match file_or_object(input, "group-spec")? {
        Some(v @ Value::Object(_)) => unprefixed(&v),
        Some(_) => return Err("group-spec: the group's settings, as a file or an object".into()),
        None => json!({}),
    };
    let m = spec.as_object_mut().ok_or("group-spec: an object")?;
    for k in GROUP_OUTPUT {
        m.remove(*k);
    }
    m.entry("baseInstanceName").or_insert_with(|| json!(group));
    m.entry("targetSize").or_insert_with(|| json!(1));
    m.insert("versions".into(), json!([{ "instanceTemplate": made }]));
    let mut update = json!({ "type": "PROACTIVE", "minimalAction": "REPLACE" });
    if let (Some(Value::Object(u)), Value::Object(to)) = (input.spec.get("update"), &mut update) {
        for (k, v) in u {
            to.insert(k.clone(), v.clone());
        }
    }
    m.insert("updatePolicy".into(), update);
    Ok(vec![wave(object(GROUP, "InstanceTemplate", base, None, template), 0), wave(object(GROUP, "InstanceGroupManager", group, None, spec), 1)])
}

// ------------------------------------------------------------------ Compute Engine's answers → objects

/// An InstanceTemplate object from the template kiln made last (`answer`): it reads as kiln made
/// it (`applied`), as templates never change.
pub fn live_template(base: &str, answer: &Value, applied: Option<&Value>) -> Option<Value> {
    let name = answer.get("name")?.as_str()?;
    let spec = match applied.and_then(|a| a.get("spec")).filter(|s| template_name(base, s) == name) {
        Some(s) => s.clone(),
        None => json!({ "properties": unprefixed(answer.get("properties").unwrap_or(&Value::Null)) }),
    };
    let mut o = object(GROUP, "InstanceTemplate", base, None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "name": name, "creationTimestamp": answer.get("creationTimestamp") }));
    }
    Some(o)
}

/// An InstanceGroupManager object from Compute Engine's answer (with `errors`: listErrors' items
/// since kiln's last change, when it isn't stable).
pub fn live_group(answer: &Value) -> Option<Value> {
    let name = answer.get("name")?.as_str()?;
    let mut spec = Map::new();
    for (k, v) in answer.as_object()? {
        if !GROUP_OUTPUT.contains(&k.as_str()) && !v.is_null() {
            spec.insert(k.clone(), unprefixed(v));
        }
    }
    if let Some(Value::Array(vs)) = spec.get_mut("versions") {
        for v in vs.iter_mut() {
            if let Some(t) = v.get("instanceTemplate").and_then(Value::as_str).map(|t| short(t).to_owned())
                && let Some(m) = v.as_object_mut()
            {
                m.insert("instanceTemplate".into(), json!(t));
            }
        }
    }
    let mut o = object(GROUP, "InstanceGroupManager", name, None, Value::Object(spec));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "isStable": answer.pointer("/status/isStable"), "versionTargetReached": answer.pointer("/status/versionTarget/isReached"), "currentActions": answer.get("currentActions"), "errors": answer.get("errors") }));
    }
    Some(o)
}

/// The group kiln sends: what `desired` sets, its templates as references in `project`.
pub fn group_body(desired: &Value, project: &str) -> Value {
    let mut body = desired.get("spec").cloned().unwrap_or_else(|| json!({}));
    if let Some(Value::Array(vs)) = body.get_mut("versions") {
        for v in vs.iter_mut() {
            if let Some(t) = v.get("instanceTemplate").and_then(Value::as_str).map(str::to_owned)
                && let Some(m) = v.as_object_mut()
            {
                m.insert("instanceTemplate".into(), json!(format!("projects/{project}/global/instanceTemplates/{}", short(&t))));
            }
        }
    }
    body
}

pub fn health(o: &Value) -> Option<Status> {
    match crate::obj::kind(o) {
        "InstanceTemplate" => status(Health::Healthy, ""),
        "InstanceGroupManager" => {
            let st = o.get("status")?;
            let stable = st.get("isStable").and_then(Value::as_bool) == Some(true);
            let reached = st.get("versionTargetReached").and_then(Value::as_bool) == Some(true);
            let errors: Vec<String> = st.get("errors").and_then(Value::as_array).into_iter().flatten().filter_map(|e| e.pointer("/error/message").and_then(Value::as_str).map(str::to_owned)).collect();
            if stable && reached {
                return status(Health::Healthy, "");
            }
            if let Some(e) = errors.first() {
                return status(Health::Degraded, format!("its instances fail: {e}"));
            }
            let doing: Vec<String> = st.get("currentActions").and_then(Value::as_object).into_iter().flatten().filter(|(k, v)| *k != "none" && v.as_u64().is_some_and(|n| n > 0)).map(|(k, v)| format!("{k} {v}")).collect();
            status(Health::Progressing, if doing.is_empty() { "rolling out".to_owned() } else { format!("rolling out: {}", doing.join(", ")) })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_named_for_their_properties() -> Result<(), String> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let read = |_: &str| None;
        let spec = |image: &str| json!({ "group": "web", "template": "web", "image": image, "properties": { "machineType": "e2-medium", "networkInterfaces": [{ "network": "https://www.googleapis.com/compute/v1/projects/acme/global/networks/default" }] }, "group-spec": { "targetSize": 3 }, "update": { "maxSurge": { "fixed": 1 } } });
        let render_of = |s: &Value| render(&RenderInput { spec: s, target: &EMPTY, app: "default/web-staging", namespace: "", revision: "c0ffee", read: &read });
        let a = render_of(&spec("projects/acme/global/images/web-1"))?;
        let (t, g) = (a.first().ok_or("template")?, a.get(1).ok_or("group")?);
        assert_eq!(t.pointer("/spec/properties/disks/0"), Some(&json!({ "boot": true, "autoDelete": true, "initializeParams": { "sourceImage": "projects/acme/global/images/web-1" } })));
        assert_eq!(t.pointer("/spec/properties/networkInterfaces/0/network"), Some(&json!("projects/acme/global/networks/default")), "references without the URL prefix");
        let made = template_name("web", t.get("spec").ok_or("spec")?);
        assert_eq!(g.pointer("/spec/versions"), Some(&json!([{ "instanceTemplate": made }])));
        assert_eq!(g.pointer("/spec/updatePolicy"), Some(&json!({ "type": "PROACTIVE", "minimalAction": "REPLACE", "maxSurge": { "fixed": 1 } })));
        assert_eq!((g.pointer("/spec/targetSize"), g.pointer("/spec/baseInstanceName")), (Some(&json!(3)), Some(&json!("web"))));
        let b = render_of(&spec("projects/acme/global/images/web-2"))?;
        assert_ne!(b.get(1).and_then(|g| g.pointer("/spec/versions/0/instanceTemplate")), Some(&json!(made)), "another image, another template");
        assert_eq!(group_body(g, "acme").pointer("/versions/0/instanceTemplate"), Some(&json!(format!("projects/acme/global/instanceTemplates/{made}"))));
        Ok(())
    }
}
