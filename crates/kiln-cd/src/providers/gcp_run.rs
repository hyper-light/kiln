//! `gcp-run` (docs/cd-targets.md §3): a Cloud Run service, each release's template a revision of
//! its own that takes all the traffic once it's ready.
//!
//! ```yaml
//! shop:
//!   gcp-run:
//!     service: shop
//!     image: "us-docker.pkg.dev/acme/shop/web@${{ release.artifacts.image.digest }}"
//!     spec: deploy/service.yaml          # a service.yaml, or Cloud Run's v2 Service; or an object
//!     set: { "template.containers[0].env[name=MODE].value": prod }
//! ```
//!
//! - **Service** (`shop`): `spec` is the Service of Cloud Run's Admin API v2, as it spells it:
//!   `template` (the revision's), `traffic`, `ingress`, `scaling`, `labels`, …. `image` is the
//!   first container's. kiln names the template's revision for its content (`<service>-k<hash>`),
//!   so the same template is the same revision, and gives that revision all the traffic. A
//!   rollback moves the traffic back to its revision, which Cloud Run kept: nothing is made again.
//! - **service.yaml.** A file in Knative's shape (`apiVersion: serving.knative.dev/v1`, as
//!   `gcloud run services replace` takes it) is read into v2's: its containers, concurrency,
//!   timeout, service account, volumes, and the annotations v2 has fields for. One kiln can't
//!   read is refused, never dropped.
//! - **Health** is the routes': the service serves once the revisions its traffic goes to are
//!   ready. A newer revision that failed and holds no traffic (after a rollback) leaves it
//!   healthy, and says so.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "run.gcp.kiln.dev";

/// A traffic target that names its revision, and one that follows the newest ready revision.
pub const REVISION: &str = "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION";
pub const LATEST: &str = "TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST";

/// The label kiln's app label goes in on Google Cloud: Google's label keys hold no `.` or `/`.
pub const APP_LABEL: &str = "kiln-app";

/// A Service's fields Cloud Run sets, which kiln never does.
pub const OUTPUT: &[&str] = &[
    "name",
    "uid",
    "generation",
    "createTime",
    "updateTime",
    "deleteTime",
    "expireTime",
    "creator",
    "lastModifier",
    "observedGeneration",
    "terminalCondition",
    "conditions",
    "latestReadyRevision",
    "latestCreatedRevision",
    "trafficStatuses",
    "uri",
    "urls",
    "satisfiesPzs",
    "reconciling",
    "etag",
];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["spec"])
}

/// Whether `s` is a name Google Cloud takes for a resource: lowercase letters, digits and `-`,
/// starting with a letter and not ending with `-`, at most `max` long.
pub fn valid_name(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    b.len() <= max && b.first().is_some_and(u8::is_ascii_lowercase) && b.last().is_some_and(|c| *c != b'-') && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A resource name's last part: `projects/p/locations/l/services/s/revisions/r` → `r`.
pub fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The revision a template is, by its content: `<service>-k<hash>`.
pub fn revision_name(service: &str, template: &Value) -> String {
    let mut t = template.clone();
    if let Some(m) = t.as_object_mut() {
        m.remove("revision");
    }
    format!("{service}-k{}", &format!("{:016x}", crate::obj::hash_value(&t))[..10])
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let service = text(input.spec, "service").ok_or("gcp-run needs service: the service's name")?;
    if !valid_name(service, 49) {
        return Err(format!("service `{service}`: up to 49 lowercase letters, digits and -, starting with a letter and not ending with -"));
    }
    let mut svc = match file_or_object(input, "spec")? {
        Some(v) if is_knative(&v) => from_knative(&v)?,
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err("spec: a Service (Cloud Run's v2, or a service.yaml), as a file or an object".into()),
        None => json!({}),
    };
    let m = svc.as_object_mut().ok_or("spec: a Service")?;
    for k in OUTPUT {
        m.remove(*k);
    }
    // The traffic is kiln's: all of it to the release's revision.
    m.remove("traffic");
    if let Some(image) = text(input.spec, "image") {
        let template = m.entry("template").or_insert_with(|| json!({}));
        let containers = crate::obj::ensure_map(template, &[]).ok_or("spec.template: an object")?.entry("containers").or_insert_with(|| json!([]));
        let Value::Array(list) = containers else { return Err("spec.template.containers: a list".into()) };
        if list.is_empty() {
            list.push(json!({}));
        }
        match list.first_mut() {
            Some(Value::Object(c)) => {
                c.insert("image".into(), Value::String(image.to_owned()));
            }
            _ => return Err("spec.template.containers[0]: an object".into()),
        }
    }
    apply_sets(&mut svc, input.spec.get("set"))?;
    let template = svc.get_mut("template").and_then(Value::as_object_mut).ok_or("gcp-run needs image: the image to run (${{ release.artifacts.<name>.digest }}), or a template in spec")?;
    let has_image = template.get("containers").and_then(Value::as_array).and_then(|c| c.first()).and_then(|c| c.get("image")).and_then(Value::as_str).is_some_and(|i| !i.trim().is_empty());
    if !has_image {
        return Err("gcp-run needs image: the image to run (${{ release.artifacts.<name>.digest }}), or the template's first container's in spec".into());
    }
    template.remove("revision");
    let revision = revision_name(service, &Value::Object(template.clone()));
    template.insert("revision".into(), Value::String(revision.clone()));
    if let Value::Object(m) = &mut svc {
        m.insert("traffic".into(), json!([{ "type": REVISION, "revision": revision, "percent": 100 }]));
    }
    Ok(vec![object(GROUP, "Service", service, None, svc)])
}

// ------------------------------------------------------------------ Knative's service.yaml → v2

fn is_knative(v: &Value) -> bool {
    v.get("apiVersion").and_then(Value::as_str).is_some_and(|a| a.starts_with("serving.knative.dev/"))
}

/// Annotations and labels Cloud Run and Knative set or read, never a person's own.
fn system(key: &str) -> bool {
    ["run.googleapis.com/", "serving.knative.dev/", "autoscaling.knative.dev/", "cloud.googleapis.com/", "client.knative.dev/", "networking.knative.dev/"].iter().any(|p| key.starts_with(p))
}

fn user_map(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).into_iter().flatten().filter(|(k, _)| !system(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn as_text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

fn int_of(key: &str, v: &Value) -> Result<i64, String> {
    as_text(v).trim().parse::<i64>().map_err(|_| format!("{key}: a whole number"))
}

fn bool_of(key: &str, v: &Value) -> Result<bool, String> {
    match as_text(v).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key}: true or false")),
    }
}

/// A Knative container as v2's: the same fields, but secrets' references.
fn container_of(c: &Value) -> Result<Value, String> {
    let Value::Object(c) = c else { return Err("spec.template.spec.containers: objects".into()) };
    let mut out = Map::new();
    for k in ["name", "image", "command", "args", "workingDir", "ports", "volumeMounts", "livenessProbe", "startupProbe"] {
        if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(l) = c.get("resources").and_then(|r| r.get("limits")).filter(|v| !v.is_null()) {
        out.insert("resources".into(), json!({ "limits": l }));
    }
    if let Some(Value::Array(env)) = c.get("env") {
        let mut list = Vec::new();
        for e in env {
            let name = e.get("name").cloned().unwrap_or(Value::Null);
            match e.pointer("/valueFrom/secretKeyRef") {
                Some(r) => list.push(json!({ "name": name, "valueSource": { "secretKeyRef": { "secret": r.get("name"), "version": r.get("key").cloned().unwrap_or(json!("latest")) } } })),
                None if e.get("valueFrom").is_some() => return Err(format!("env {}: only secretKeyRef is read from valueFrom", as_text(&name))),
                None => list.push(json!({ "name": name, "value": e.get("value").cloned().unwrap_or(json!("")) })),
            }
        }
        out.insert("env".into(), Value::Array(list));
    }
    Ok(Value::Object(out))
}

/// A Knative volume as v2's: secrets and in-memory directories.
fn volume_of(v: &Value) -> Result<Value, String> {
    let name = v.get("name").cloned().unwrap_or(Value::Null);
    if let Some(s) = v.get("secret") {
        let mut secret = Map::new();
        secret.insert("secret".into(), s.get("secretName").cloned().unwrap_or(Value::Null));
        if let Some(Value::Array(items)) = s.get("items") {
            let items: Vec<Value> = items.iter().map(|i| json!({ "version": i.get("key").cloned().unwrap_or(json!("latest")), "path": i.get("path") })).collect();
            secret.insert("items".into(), Value::Array(items));
        }
        if let Some(m) = s.get("defaultMode") {
            secret.insert("defaultMode".into(), m.clone());
        }
        return Ok(json!({ "name": name, "secret": secret }));
    }
    if let Some(e) = v.get("emptyDir") {
        let mut d = Map::new();
        if e.get("medium").and_then(Value::as_str) == Some("Memory") {
            d.insert("medium".into(), json!("MEMORY"));
        }
        if let Some(l) = e.get("sizeLimit") {
            d.insert("sizeLimit".into(), l.clone());
        }
        return Ok(json!({ "name": name, "emptyDir": d }));
    }
    Err(format!("volume {}: kiln reads secret and emptyDir volumes from a service.yaml; write others in Cloud Run's v2 shape", as_text(&name)))
}

/// A Knative service.yaml (`gcloud run services replace`'s) as Cloud Run's v2 Service.
pub fn from_knative(v: &Value) -> Result<Value, String> {
    let meta = v.get("metadata").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    let labels = user_map(meta.get("labels"));
    if !labels.is_empty() {
        out.insert("labels".into(), Value::Object(labels));
    }
    let mut notes = Map::new();
    for (k, val) in meta.get("annotations").and_then(Value::as_object).into_iter().flatten() {
        let s = as_text(val);
        match k.as_str() {
            "run.googleapis.com/ingress" => {
                let i = match s.as_str() {
                    "all" => "INGRESS_TRAFFIC_ALL",
                    "internal" => "INGRESS_TRAFFIC_INTERNAL_ONLY",
                    "internal-and-cloud-load-balancing" => "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER",
                    other => return Err(format!("{k}: `{other}` isn't all, internal or internal-and-cloud-load-balancing")),
                };
                out.insert("ingress".into(), json!(i));
            }
            "run.googleapis.com/launch-stage" => {
                out.insert("launchStage".into(), json!(s));
            }
            "run.googleapis.com/description" => {
                out.insert("description".into(), json!(s));
            }
            "run.googleapis.com/invoker-iam-disabled" => {
                out.insert("invokerIamDisabled".into(), json!(bool_of(k, val)?));
            }
            "run.googleapis.com/binary-authorization" => {
                out.insert("binaryAuthorization".into(), if s == "default" { json!({ "useDefault": true }) } else { json!({ "policy": s }) });
            }
            // Cloud Run's own, reporting what it did.
            k if system(k) => {}
            _ => {
                notes.insert(k.clone(), val.clone());
            }
        }
    }
    if !notes.is_empty() {
        out.insert("annotations".into(), Value::Object(notes));
    }
    let tm = v.pointer("/spec/template/metadata").cloned().unwrap_or(Value::Null);
    let ts = v.pointer("/spec/template/spec").cloned().unwrap_or(Value::Null);
    let mut t = Map::new();
    let labels = user_map(tm.get("labels"));
    if !labels.is_empty() {
        t.insert("labels".into(), Value::Object(labels));
    }
    let mut containers = ts.get("containers").and_then(Value::as_array).into_iter().flatten().map(container_of).collect::<Result<Vec<_>, _>>()?;
    let mut volumes = ts.get("volumes").and_then(Value::as_array).into_iter().flatten().map(volume_of).collect::<Result<Vec<_>, _>>()?;
    let (mut scaling, mut vpc, mut notes) = (Map::new(), Map::new(), Map::new());
    let each_container = |containers: &mut Vec<Value>, field: &str, v: Value| {
        for c in containers.iter_mut() {
            if let Some(r) = crate::obj::ensure_map(c, &["resources"]) {
                r.insert(field.into(), v.clone());
            }
        }
    };
    for (k, val) in tm.get("annotations").and_then(Value::as_object).into_iter().flatten() {
        let s = as_text(val);
        match k.as_str() {
            "autoscaling.knative.dev/minScale" => {
                scaling.insert("minInstanceCount".into(), json!(int_of(k, val)?));
            }
            "autoscaling.knative.dev/maxScale" => {
                scaling.insert("maxInstanceCount".into(), json!(int_of(k, val)?));
            }
            "run.googleapis.com/execution-environment" => {
                let e = match s.as_str() {
                    "gen1" => "EXECUTION_ENVIRONMENT_GEN1",
                    "gen2" => "EXECUTION_ENVIRONMENT_GEN2",
                    other => return Err(format!("{k}: `{other}` isn't gen1 or gen2")),
                };
                t.insert("executionEnvironment".into(), json!(e));
            }
            "run.googleapis.com/vpc-access-connector" => {
                vpc.insert("connector".into(), json!(s));
            }
            "run.googleapis.com/vpc-access-egress" => {
                let e = match s.as_str() {
                    "all-traffic" | "all" => "ALL_TRAFFIC",
                    "private-ranges-only" => "PRIVATE_RANGES_ONLY",
                    other => return Err(format!("{k}: `{other}` isn't all-traffic or private-ranges-only")),
                };
                vpc.insert("egress".into(), json!(e));
            }
            "run.googleapis.com/encryption-key" => {
                t.insert("encryptionKey".into(), json!(s));
            }
            "run.googleapis.com/sessionAffinity" => {
                t.insert("sessionAffinity".into(), json!(bool_of(k, val)?));
            }
            "run.googleapis.com/cpu-throttling" => each_container(&mut containers, "cpuIdle", json!(bool_of(k, val)?)),
            "run.googleapis.com/startup-cpu-boost" => each_container(&mut containers, "startupCpuBoost", json!(bool_of(k, val)?)),
            "run.googleapis.com/cloudsql-instances" => {
                let instances: Vec<Value> = s.split(',').map(str::trim).filter(|i| !i.is_empty()).map(|i| json!(i)).collect();
                volumes.push(json!({ "name": "cloudsql", "cloudSqlInstance": { "instances": instances } }));
                for c in &mut containers {
                    if let Some(Value::Array(mounts)) = crate::obj::ensure_map(c, &[]).map(|m| m.entry("volumeMounts").or_insert_with(|| json!([]))) {
                        mounts.push(json!({ "name": "cloudsql", "mountPath": "/cloudsql" }));
                    }
                }
            }
            // What made the file, and what Cloud Run reports: nothing to set.
            "run.googleapis.com/client-name" | "run.googleapis.com/client-version" | "client.knative.dev/user-image" | "run.googleapis.com/operation-id" => {}
            k if system(k) => return Err(format!("{k}: kiln doesn't read this annotation of a service.yaml yet; write the template in Cloud Run's v2 shape")),
            _ => {
                notes.insert(k.clone(), val.clone());
            }
        }
    }
    if !notes.is_empty() {
        t.insert("annotations".into(), Value::Object(notes));
    }
    if !scaling.is_empty() {
        t.insert("scaling".into(), Value::Object(scaling));
    }
    if !vpc.is_empty() {
        t.insert("vpcAccess".into(), Value::Object(vpc));
    }
    if let Some(n) = ts.get("containerConcurrency").filter(|v| !v.is_null()) {
        t.insert("maxInstanceRequestConcurrency".into(), json!(int_of("containerConcurrency", n)?));
    }
    if let Some(n) = ts.get("timeoutSeconds").filter(|v| !v.is_null()) {
        t.insert("timeout".into(), json!(format!("{}s", int_of("timeoutSeconds", n)?)));
    }
    if let Some(a) = ts.get("serviceAccountName").and_then(Value::as_str) {
        t.insert("serviceAccount".into(), json!(a));
    }
    if !containers.is_empty() {
        t.insert("containers".into(), Value::Array(containers));
    }
    if !volumes.is_empty() {
        t.insert("volumes".into(), Value::Array(volumes));
    }
    out.insert("template".into(), Value::Object(t));
    Ok(Value::Object(out))
}

// ------------------------------------------------------------------ Cloud Run's answers → objects

fn percent(t: &Value) -> u64 {
    t.get("percent").and_then(Value::as_u64).unwrap_or(0)
}

/// A traffic target as kiln compares it: its type, revision (short), percent and tag.
fn target_of(t: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), t.get("type").cloned().unwrap_or_else(|| json!(REVISION)));
    if let Some(r) = t.get("revision").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        m.insert("revision".into(), json!(short(r)));
    }
    m.insert("percent".into(), json!(percent(t)));
    if let Some(tag) = t.get("tag").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        m.insert("tag".into(), json!(tag));
    }
    Value::Object(m)
}

/// A Service object from Cloud Run's Service.
pub fn live_service(answer: &Value) -> Option<Value> {
    let name = short(answer.get("name")?.as_str()?).to_owned();
    let mut spec = Map::new();
    for (k, v) in answer.as_object()? {
        if !OUTPUT.contains(&k.as_str()) && !v.is_null() {
            spec.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Array(t)) = spec.get_mut("traffic") {
        *t = t.iter().map(target_of).collect();
    }
    if let Some(Value::Object(t)) = spec.get_mut("template")
        && let Some(r) = t.get("revision").and_then(Value::as_str).map(|r| short(r).to_owned())
    {
        t.insert("revision".into(), json!(r));
    }
    // kiln's tracking, out of Cloud Run's labels and annotations.
    let app = spec.get_mut("labels").and_then(Value::as_object_mut).and_then(|l| l.remove(APP_LABEL));
    let tracking = spec.get_mut("annotations").and_then(Value::as_object_mut).and_then(|a| a.remove(crate::engine::TRACKING));
    for k in ["labels", "annotations"] {
        if spec.get(k).and_then(Value::as_object).is_some_and(Map::is_empty) {
            spec.remove(k);
        }
    }
    let mut o = object(GROUP, "Service", &name, None, Value::Object(spec));
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
    let rev = |k: &str| answer.get(k).and_then(Value::as_str).map(|r| json!(short(r)));
    let statuses: Vec<Value> = answer.get("trafficStatuses").and_then(Value::as_array).into_iter().flatten().map(|s| {
        let mut t = target_of(s);
        if let (Value::Object(m), Some(u)) = (&mut t, s.get("uri")) {
            m.insert("uri".into(), u.clone());
        }
        t
    }).collect();
    if let Value::Object(m) = &mut o {
        m.insert(
            "status".into(),
            json!({
                "uri": answer.get("uri"), "latestReadyRevision": rev("latestReadyRevision"), "latestCreatedRevision": rev("latestCreatedRevision"),
                "terminalCondition": answer.get("terminalCondition"), "conditions": answer.get("conditions"), "trafficStatuses": statuses,
                "observedGeneration": answer.get("observedGeneration"), "generation": answer.get("generation"), "reconciling": answer.get("reconciling").and_then(Value::as_bool).unwrap_or(false),
            }),
        );
    }
    Some(o)
}

/// The revision all of a service's traffic goes to, as it's configured: a revision's own target,
/// or the latest's (the newest ready revision).
pub fn serving(o: &Value) -> Option<String> {
    let taking: Vec<&Value> = o.pointer("/spec/traffic")?.as_array()?.iter().filter(|t| percent(t) > 0).collect();
    let [t] = taking.as_slice() else { return None };
    if percent(t) != 100 {
        return None;
    }
    match t.get("type").and_then(Value::as_str) {
        Some(LATEST) => o.pointer("/status/latestReadyRevision").and_then(Value::as_str).map(str::to_owned),
        _ => t.get("revision").and_then(Value::as_str).map(|r| short(r).to_owned()),
    }
}

/// A service as kiln reads it: while all its traffic goes to the revision kiln's last apply
/// named, its template reads as that apply's. Cloud Run keeps the newest template it was given,
/// which after a rollback is a later release's; the revision serving is what kiln compares.
pub fn present(mut live: Value, applied: Option<&Value>) -> Value {
    let Some(t) = applied.and_then(|a| a.pointer("/spec/template")) else { return live };
    let Some(want) = t.get("revision").and_then(Value::as_str) else { return live };
    if serving(&live).as_deref() == Some(want)
        && let Some(spec) = live.get_mut("spec").and_then(Value::as_object_mut)
    {
        spec.insert("template".into(), t.clone());
    }
    live
}

// ------------------------------------------------------------------ objects → Cloud Run's requests

/// The labels and annotations a service is made or changed with: its own, and kiln's tracking.
fn with_tracking(body: &mut Map<String, Value>, desired: &Value) {
    if let Some(v) = crate::obj::label(desired, crate::engine::APP_LABEL) {
        let l = body.entry("labels").or_insert_with(|| json!({}));
        if let Value::Object(l) = l {
            l.insert(APP_LABEL.into(), json!(v));
        }
    }
    if let Some(v) = crate::obj::annotation(desired, crate::engine::TRACKING) {
        let a = body.entry("annotations").or_insert_with(|| json!({}));
        if let Value::Object(a) = a {
            a.insert(crate::engine::TRACKING.into(), json!(v));
        }
    }
}

/// The Service CreateService makes.
pub fn create_body(desired: &Value) -> Value {
    let mut body = desired.get("spec").and_then(Value::as_object).cloned().unwrap_or_default();
    with_tracking(&mut body, desired);
    Value::Object(body)
}

/// The Service UpdateService sends to make `live` (Cloud Run's answer) what `desired` says: the
/// service as it is, with what kiln sets over it, at the etag it was read at. With
/// `keep_template` the template stays as it is: the revision kiln wants is there already.
pub fn update_body(live: &Value, desired: &Value, keep_template: bool) -> Value {
    let mut body = live.as_object().cloned().unwrap_or_default();
    for k in OUTPUT {
        if !matches!(*k, "name" | "etag") {
            body.remove(*k);
        }
    }
    for (k, v) in desired.get("spec").and_then(Value::as_object).into_iter().flatten() {
        if k == "template" && keep_template {
            continue;
        }
        body.insert(k.clone(), v.clone());
    }
    with_tracking(&mut body, desired);
    Value::Object(body)
}

/// Whether `have` holds everything `want` says: objects key by key, lists item by item.
pub fn covers(have: &Value, want: &Value) -> bool {
    match (have, want) {
        (Value::Object(h), Value::Object(w)) => w.iter().all(|(k, v)| h.get(k).is_some_and(|x| covers(x, v)) || (v.is_null() && h.get(k).is_none())),
        (Value::Array(h), Value::Array(w)) => h.len() == w.len() && h.iter().zip(w).all(|(x, y)| covers(x, y)),
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => have == want,
    }
}

/// Whether an update would change nothing: `body` (from [`update_body`]) is what the service
/// has, with its traffic exactly so.
pub fn unchanged(live: &Value, body: &Value) -> bool {
    let traffic = |v: &Value| v.get("traffic").and_then(Value::as_array).map(|t| t.iter().map(target_of).collect::<Vec<_>>()).unwrap_or_default();
    traffic(live) == traffic(body) && covers(live, body)
}

// ------------------------------------------------------------------ health

fn state(c: Option<&Value>) -> &str {
    c.and_then(|c| c.get("state")).and_then(Value::as_str).unwrap_or_default()
}

fn message(c: Option<&Value>) -> String {
    c.and_then(|c| c.get("message")).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// What takes traffic, by revision: as configured (`spec.traffic`) or as served
/// (`status.trafficStatuses`), the latest's resolved to the newest ready revision.
fn taking(o: &Value, list: Option<&Value>) -> Vec<(String, u64)> {
    let latest = o.pointer("/status/latestReadyRevision").and_then(Value::as_str).unwrap_or_default();
    let mut out: Vec<(String, u64)> = list
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| percent(t) > 0)
        .map(|t| {
            let r = match t.get("type").and_then(Value::as_str) {
                Some(LATEST) => latest.to_owned(),
                _ => t.get("revision").and_then(Value::as_str).map(short).unwrap_or_default().to_owned(),
            };
            (r, percent(t))
        })
        .collect();
    out.sort();
    out
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "Service" {
        return None;
    }
    let st = o.get("status")?;
    if st.get("reconciling").and_then(Value::as_bool) == Some(true) {
        return status(Health::Progressing, "Cloud Run is rolling the service out");
    }
    let cond = |t: &str| st.get("conditions").and_then(Value::as_array).and_then(|c| c.iter().find(|c| c.get("type").and_then(Value::as_str) == Some(t)));
    let routes = cond("RoutesReady");
    if state(routes) == "CONDITION_FAILED" {
        return status(Health::Degraded, message(routes));
    }
    let served = taking(o, st.get("trafficStatuses")) == taking(o, o.pointer("/spec/traffic"));
    let terminal = st.get("terminalCondition");
    match state(terminal) {
        "CONDITION_SUCCEEDED" if served => status(Health::Healthy, ""),
        "CONDITION_SUCCEEDED" => status(Health::Progressing, "the traffic hasn't moved yet"),
        // Only a revision with no traffic failed (a rollback moved it off): what's asked serves.
        "CONDITION_FAILED" if served && state(routes) == "CONDITION_SUCCEEDED" => {
            status(Health::Healthy, format!("serving {}; the newest revision failed: {}", serving(o).unwrap_or_default(), message(terminal)))
        }
        "CONDITION_FAILED" => status(Health::Degraded, message(terminal)),
        _ => status(Health::Progressing, { let m = message(terminal); if m.is_empty() { "waiting for Cloud Run".to_owned() } else { m } }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(spec: &'a Value, read: &'a dyn Fn(&str) -> Option<Vec<u8>>) -> RenderInput<'a> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        RenderInput { spec, target: &EMPTY, app: "default/shop-staging", namespace: "", revision: "c0ffee", read }
    }

    /// A service.yaml as `gcloud run services replace` takes it reads as v2's Service; the same
    /// template is the same revision, and all the traffic goes to it.
    #[test]
    fn service_yamls_read_as_v2() -> Result<(), String> {
        let yaml = r#"
apiVersion: serving.knative.dev/v1
kind: Service
metadata:
  name: shop
  labels: { team: shop, cloud.googleapis.com/location: us-central1 }
  annotations: { run.googleapis.com/ingress: internal-and-cloud-load-balancing, run.googleapis.com/operation-id: x }
spec:
  template:
    metadata:
      annotations:
        autoscaling.knative.dev/maxScale: "20"
        autoscaling.knative.dev/minScale: "1"
        run.googleapis.com/execution-environment: gen2
        run.googleapis.com/cpu-throttling: "false"
        run.googleapis.com/cloudsql-instances: acme:us-central1:db
    spec:
      containerConcurrency: 40
      timeoutSeconds: 120
      serviceAccountName: shop@acme.iam.gserviceaccount.com
      containers:
      - image: us-docker.pkg.dev/acme/shop/web:old
        ports: [{ name: http1, containerPort: 8080 }]
        env:
        - { name: MODE, value: prod }
        - { name: DB_PASSWORD, valueFrom: { secretKeyRef: { name: db-password, key: latest } } }
        resources: { limits: { cpu: "2", memory: 1Gi }, requests: { cpu: "1" } }
  traffic: [{ percent: 100, latestRevision: true }]
"#;
        let read = |p: &str| (p == "service.yaml").then(|| yaml.as_bytes().to_vec());
        let spec = json!({ "service": "shop", "spec": "service.yaml", "image": "us-docker.pkg.dev/acme/shop/web@sha256:aa" });
        let objs = render(&input(&spec, &read))?;
        let s = objs.first().ok_or("no service")?;
        assert_eq!(s.pointer("/spec/ingress"), Some(&json!("INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER")));
        assert_eq!(s.pointer("/spec/labels"), Some(&json!({ "team": "shop" })));
        let t = s.pointer("/spec/template").ok_or("no template")?;
        assert_eq!(t.get("scaling"), Some(&json!({ "minInstanceCount": 1, "maxInstanceCount": 20 })));
        assert_eq!((t.get("maxInstanceRequestConcurrency"), t.get("timeout")), (Some(&json!(40)), Some(&json!("120s"))));
        assert_eq!(t.pointer("/containers/0/image"), Some(&json!("us-docker.pkg.dev/acme/shop/web@sha256:aa")), "image: wins");
        assert_eq!(t.pointer("/containers/0/resources"), Some(&json!({ "limits": { "cpu": "2", "memory": "1Gi" }, "cpuIdle": false })));
        assert_eq!(t.pointer("/containers/0/env/1/valueSource/secretKeyRef"), Some(&json!({ "secret": "db-password", "version": "latest" })));
        assert_eq!(t.pointer("/volumes/0/cloudSqlInstance/instances"), Some(&json!(["acme:us-central1:db"])));
        assert_eq!(t.pointer("/containers/0/volumeMounts/0/mountPath"), Some(&json!("/cloudsql")));
        let rev = t.get("revision").and_then(Value::as_str).ok_or("no revision")?;
        assert!(rev.starts_with("shop-k") && valid_name(rev, 63), "{rev}");
        assert_eq!(s.pointer("/spec/traffic"), Some(&json!([{ "type": REVISION, "revision": rev, "percent": 100 }])));
        // The same template, the same revision; another image, another.
        let again = render(&input(&spec, &read))?;
        assert_eq!(again.first().and_then(|o| o.pointer("/spec/template/revision")), Some(&json!(rev)));
        let other = json!({ "service": "shop", "spec": "service.yaml", "image": "us-docker.pkg.dev/acme/shop/web@sha256:bb" });
        assert_ne!(render(&input(&other, &read))?.first().and_then(|o| o.pointer("/spec/template/revision")), Some(&json!(rev)));
        // An annotation kiln can't read is refused, never dropped.
        let odd = yaml.replace("run.googleapis.com/cpu-throttling", "run.googleapis.com/network-interfaces");
        let read_odd = |p: &str| (p == "service.yaml").then(|| odd.as_bytes().to_vec());
        assert!(render(&input(&spec, &read_odd)).is_err_and(|e| e.contains("network-interfaces")));
        Ok(())
    }

    /// Health follows the routes: a failed newest revision with no traffic leaves the service
    /// healthy; traffic asked of one that failed is degraded.
    #[test]
    fn health_follows_what_serves() {
        let svc = |traffic: Value, statuses: Value, terminal: &str, routes: &str| {
            json!({ "apiVersion": "run.gcp.kiln.dev/v1", "kind": "Service", "metadata": { "name": "shop" }, "spec": { "traffic": traffic },
                "status": { "latestReadyRevision": "shop-b", "trafficStatuses": statuses, "terminalCondition": { "type": "Ready", "state": terminal, "message": "Revision 'shop-c' is not ready" },
                    "conditions": [{ "type": "RoutesReady", "state": routes, "message": "Revision 'shop-c' is not ready" }], "reconciling": false } })
        };
        let on = |r: &str| json!([{ "type": REVISION, "revision": r, "percent": 100 }]);
        let h = |o: &Value| health(o).map(|s| s.health);
        assert_eq!(h(&svc(on("shop-b"), on("shop-b"), "CONDITION_SUCCEEDED", "CONDITION_SUCCEEDED")), Some(Health::Healthy));
        assert_eq!(h(&svc(on("shop-c"), on("shop-b"), "CONDITION_FAILED", "CONDITION_FAILED")), Some(Health::Degraded));
        let back = svc(on("shop-b"), on("shop-b"), "CONDITION_FAILED", "CONDITION_SUCCEEDED");
        assert_eq!(h(&back), Some(Health::Healthy));
        assert!(health(&back).is_some_and(|s| s.message.contains("serving shop-b")));
        assert_eq!(h(&svc(on("shop-b"), on("shop-a"), "CONDITION_SUCCEEDED", "CONDITION_SUCCEEDED")), Some(Health::Progressing));
        let latest = json!([{ "type": LATEST, "percent": 100 }]);
        assert_eq!(h(&svc(latest.clone(), latest, "CONDITION_SUCCEEDED", "CONDITION_SUCCEEDED")), Some(Health::Healthy));
    }
}
