//! `gcp-functions` (docs/cd-targets.md §3): a Cloud Run function (Cloud Functions' v2 API), built
//! from the source CI zipped.
//!
//! ```yaml
//! thumbs:
//!   gcp-functions:
//!     function: thumbs
//!     source: "${{ release.artifacts.source.digest }}"   # a zip CI built, or gs://bucket/object.zip
//!     runtime: nodejs22
//!     entry-point: handler
//!     spec: deploy/function.yaml                          # the Function: serviceConfig, eventTrigger, labels, …
//!     set: { "serviceConfig.environmentVariables.MODE": prod }
//! ```
//!
//! - **Function** (`thumbs`): `spec` is the Function of Cloud Functions' v2 API, as it spells
//!   it, with `buildConfig.source` as `{artifact: sha256:…}` (a zip CI built, which kiln uploads
//!   to the function's upload URL) or `{storageSource: {bucket, object}}`. Cloud Functions builds
//!   the source and rolls the new revision out: it takes all the traffic once it's ready, and a
//!   build or start that fails leaves the one before serving. A rollback deploys the earlier
//!   release's source again. Google doesn't say where an upload came from, so kiln reads back
//!   the source it last applied while the function still builds from what that apply uploaded;
//!   another source is drift.
//! - **Health** is the function's state: `ACTIVE`, `DEPLOYING`, `FAILED` with its messages.

use serde_json::{Map, Value, json};

use super::gcp_run::{APP_LABEL, short, valid_name};
use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "functions.gcp.kiln.dev";

/// A Function's fields Cloud Functions sets, which kiln never does: at the top, and in its
/// build's, service's and trigger's configuration.
const OUTPUT: &[&str] = &["state", "updateTime", "createTime", "url", "stateMessages", "satisfiesPzs", "upgradeInfo"];
const BUILD_OUTPUT: &[&str] = &["build", "sourceProvenance"];
const SERVICE_OUTPUT: &[&str] = &["service", "uri", "revision"];
const TRIGGER_OUTPUT: &[&str] = &["trigger"];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["spec"])
}

/// A function's source as `source:` says it: `sha256:<hex>` (a zip CI built) or
/// `gs://bucket/object`.
pub fn source_of(s: &str) -> Result<Value, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Ok(json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) }));
    }
    if let Some(rest) = s.strip_prefix("gs://")
        && let Some((bucket, object)) = rest.split_once('/').filter(|(b, o)| !b.is_empty() && !o.is_empty())
    {
        return Ok(json!({ "storageSource": { "bucket": bucket, "object": object } }));
    }
    Err(format!("`{s}` is no source Cloud Functions takes: a zip CI built (${{{{ release.artifacts.<name>.digest }}}}), or gs://bucket/object"))
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let function = text(input.spec, "function").ok_or("gcp-functions needs function: the function's name")?;
    if !valid_name(function, 63) {
        return Err(format!("function `{function}`: up to 63 lowercase letters, digits and -, starting with a letter and not ending with -"));
    }
    let mut f = match file_or_object(input, "spec")? {
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err("spec: a Function (Cloud Functions' v2), as a file or an object".into()),
        None => json!({}),
    };
    let source = source_of(text(input.spec, "source").ok_or("gcp-functions needs source: a zip CI built (${{ release.artifacts.<name>.digest }}), or gs://bucket/object")?)?;
    let m = f.as_object_mut().ok_or("spec: a Function")?;
    for k in OUTPUT.iter().chain(&["name"]) {
        m.remove(*k);
    }
    m.entry("environment").or_insert_with(|| json!("GEN_2"));
    let build = crate::obj::ensure_map(&mut f, &["buildConfig"]).ok_or("spec.buildConfig: an object")?;
    for k in BUILD_OUTPUT {
        build.remove(*k);
    }
    build.insert("source".into(), source);
    if let Some(r) = text(input.spec, "runtime") {
        build.insert("runtime".into(), json!(r));
    }
    if let Some(e) = text(input.spec, "entry-point") {
        build.insert("entryPoint".into(), json!(e));
    }
    if !build.get("runtime").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
        return Err("gcp-functions needs runtime: the function's runtime (nodejs22, python312, go123, …)".into());
    }
    apply_sets(&mut f, input.spec.get("set"))?;
    Ok(vec![object(GROUP, "Function", function, None, f)])
}

// ------------------------------------------------------------------ Cloud Functions' answers → objects

fn without(v: Option<&Value>, fields: &[&str]) -> Option<Value> {
    let mut m = v?.as_object()?.clone();
    for k in fields {
        m.remove(*k);
    }
    Some(Value::Object(m))
}

/// The function as kiln changes it: Cloud Functions' answer without what it sets itself.
pub fn settable(answer: &Value) -> Map<String, Value> {
    let mut m = answer.as_object().cloned().unwrap_or_default();
    for k in OUTPUT {
        m.remove(*k);
    }
    for (k, fields) in [("buildConfig", BUILD_OUTPUT), ("serviceConfig", SERVICE_OUTPUT), ("eventTrigger", TRIGGER_OUTPUT)] {
        if let Some(v) = without(m.get(k), fields) {
            m.insert(k.into(), v);
        }
    }
    m
}

/// A Function object from Cloud Functions' answer. `applied` is what kiln last applied and what
/// that apply uploaded (`made.storageSource`): while the function builds from that upload, its
/// source reads as the one kiln applied.
pub fn live_function(answer: &Value, applied: Option<(&Value, &Value)>) -> Option<Value> {
    let name = short(answer.get("name")?.as_str()?).to_owned();
    let mut spec = settable(answer);
    spec.remove("name");
    let live_source = answer.pointer("/buildConfig/source/storageSource");
    if let Some((object, made)) = applied
        && let (Some(have), Some(uploaded)) = (live_source, made.get("storageSource"))
        && same_upload(have, uploaded)
        && let (Some(src), Some(build)) = (object.pointer("/spec/buildConfig/source"), spec.get_mut("buildConfig").and_then(Value::as_object_mut))
    {
        build.insert("source".into(), src.clone());
    }
    let app = spec.get_mut("labels").and_then(Value::as_object_mut).and_then(|l| l.remove(APP_LABEL));
    if spec.get("labels").and_then(Value::as_object).is_some_and(Map::is_empty) {
        spec.remove("labels");
    }
    let mut o = object(GROUP, "Function", &name, None, Value::Object(spec));
    if let Some(Value::String(a)) = app
        && let Some(l) = crate::obj::ensure_map(&mut o, &["metadata", "labels"])
    {
        l.insert(crate::engine::APP_LABEL.into(), json!(a));
    }
    if let Value::Object(m) = &mut o {
        m.insert(
            "status".into(),
            json!({
                "state": answer.get("state"), "stateMessages": answer.get("stateMessages"), "url": answer.get("url"), "updateTime": answer.get("updateTime"),
                "revision": answer.pointer("/serviceConfig/revision"), "service": answer.pointer("/serviceConfig/service").and_then(Value::as_str).map(short), "build": answer.pointer("/buildConfig/build"),
            }),
        );
    }
    Some(o)
}

/// Whether a function builds from the object an upload made (its bucket and name; the
/// generation, where both say one).
pub fn same_upload(have: &Value, uploaded: &Value) -> bool {
    let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str().map(str::to_owned).or_else(|| x.as_i64().map(|n| n.to_string())));
    s(have, "bucket").is_some() && s(have, "bucket") == s(uploaded, "bucket") && s(have, "object") == s(uploaded, "object") && (s(uploaded, "generation").is_none() || s(have, "generation") == s(uploaded, "generation"))
}

// ------------------------------------------------------------------ objects → Cloud Functions' requests

/// `want` merged into `base`: objects key by key, anything else replaced.
pub fn merged(base: &Value, want: &Value) -> Value {
    match (base, want) {
        (Value::Object(b), Value::Object(w)) => {
            let mut out = b.clone();
            for (k, v) in w {
                let next = match out.get(k) {
                    Some(old) => merged(old, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), next);
            }
            Value::Object(out)
        }
        _ => want.clone(),
    }
}

/// The Function kiln sends: what it has now (`live`, Cloud Functions' answer) with what
/// `desired` sets over it, building from `storage` (where its source is in Cloud Storage), and
/// kiln's app label.
pub fn function_body(live: Option<&Value>, desired: &Value, storage: &Value) -> Value {
    let base = live.map(|l| Value::Object(settable(l))).unwrap_or_else(|| json!({}));
    let mut body = merged(&base, desired.get("spec").unwrap_or(&Value::Null));
    if let Some(b) = crate::obj::ensure_map(&mut body, &["buildConfig"]) {
        b.insert("source".into(), json!({ "storageSource": storage }));
    }
    if let Some(v) = crate::obj::label(desired, crate::engine::APP_LABEL)
        && let Some(l) = crate::obj::ensure_map(&mut body, &["labels"])
    {
        l.insert(APP_LABEL.into(), json!(v));
    }
    body
}

// ------------------------------------------------------------------ health

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Function" {
        return None;
    }
    let messages: Vec<String> = o.pointer("/status/stateMessages").and_then(Value::as_array).into_iter().flatten().filter_map(|m| m.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
    match o.pointer("/status/state").and_then(Value::as_str).unwrap_or_default() {
        "ACTIVE" => status(Health::Healthy, messages.join("; ")),
        "DEPLOYING" => status(Health::Progressing, "Cloud Functions is building and rolling it out"),
        "DELETING" => status(Health::Progressing, "being deleted"),
        "FAILED" | "UNKNOWN" => status(Health::Degraded, if messages.is_empty() { "the function's deployment failed".to_owned() } else { messages.join("; ") }),
        _ => status(Health::Progressing, "waiting for Cloud Functions"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_and_bodies() -> Result<(), String> {
        let hex = "ab".repeat(32);
        assert_eq!(source_of(&format!("sha256:{hex}"))?, json!({ "artifact": format!("sha256:{hex}") }));
        assert_eq!(source_of("gs://b/src/f.zip")?, json!({ "storageSource": { "bucket": "b", "object": "src/f.zip" } }));
        assert!(source_of("f.zip").is_err());
        // Kiln's settings over what the function has; the upload as its source.
        let live = json!({ "name": "projects/p/locations/l/functions/f", "state": "ACTIVE", "buildConfig": { "runtime": "nodejs20", "dockerRepository": "projects/p/locations/l/repositories/gcf-artifacts", "build": "b1" },
            "serviceConfig": { "availableMemory": "256M", "uri": "https://f", "revision": "f-00001" } });
        let desired = json!({ "metadata": { "name": "f", "labels": { "kiln.dev/app": "00ff" } }, "spec": { "buildConfig": { "runtime": "nodejs22", "source": { "artifact": "sha256:x" } }, "serviceConfig": { "availableMemory": "512M" } } });
        let body = function_body(Some(&live), &desired, &json!({ "bucket": "up", "object": "1.zip" }));
        assert_eq!(body.pointer("/buildConfig/runtime"), Some(&json!("nodejs22")));
        assert_eq!(body.pointer("/buildConfig/dockerRepository"), Some(&json!("projects/p/locations/l/repositories/gcf-artifacts")), "what kiln doesn't set stays");
        assert_eq!(body.pointer("/buildConfig/source"), Some(&json!({ "storageSource": { "bucket": "up", "object": "1.zip" } })));
        assert_eq!((body.pointer("/buildConfig/build"), body.pointer("/serviceConfig/revision"), body.get("state")), (None, None, None), "nothing Google sets: {body}");
        assert_eq!(body.pointer("/labels/kiln-app"), Some(&json!("00ff")));
        Ok(())
    }
}
