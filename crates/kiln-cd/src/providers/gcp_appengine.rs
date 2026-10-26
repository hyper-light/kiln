//! `gcp-appengine` (docs/cd-targets.md §3): an App Engine service, each release a version of its
//! own, and the service's traffic moved to it.
//!
//! ```yaml
//! web:
//!   gcp-appengine:
//!     service: web                                      # `default`, for the app's first service
//!     source: "${{ release.artifacts.site.digest }}"    # a zip CI built, or gs://bucket/object.zip
//!     app: app.yaml                                     # app.yaml in the source, or the Version as an object
//!     set: { "envVariables.MODE": prod }
//! ```
//!
//! - **Version** (`web:kiln-<hash>`, wave 0): the Version of App Engine's Admin API as it spells
//!   it, with `deployment` as `{zip: {artifact: sha256:…}}` (a zip CI built, which kiln puts in
//!   the app's staging bucket first, or `bucket:`), `{zip: {sourceUrl}}`, or `{container: {image}}`
//!   for the flexible environment (`image:`). Named for its content (or `version:`), so the same
//!   release is the same version, which App Engine keeps: a rollback makes and uploads nothing.
//!   A version doesn't change once made; other content is another version.
//! - **Service** (`web`, wave 1): its traffic `split`, all of it to the release's version, as
//!   `gcloud app deploy --promote` does. App Engine moves it at once.
//! - **app.yaml** is read into the Admin API's Version: its settings in the API's names, the
//!   scaling settings where the API keeps them, `entrypoint` as a shell command, and handlers'
//!   scripts and static files.
//! - **Health**: a version is healthy while it serves (`servingStatus`).

use serde_json::{Map, Value, json};

use super::gcp_run::valid_name;
use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "appengine.gcp.kiln.dev";

/// A Version's fields App Engine sets, which kiln never does.
const OUTPUT: &[&str] = &["name", "id", "createTime", "createdBy", "diskUsageBytes", "versionUrl", "servingStatus", "deployment", "instances"];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["app"])
}

/// A Version object's name: `<service>:<version>`.
pub fn version_name(service: &str, id: &str) -> String {
    format!("{service}:{id}")
}

/// `service:version`, a Version's name, apart.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once(':')
}

/// A version's id for its content: `kiln-<hash>`.
pub fn version_id(version: &Value) -> String {
    format!("kiln-{}", &format!("{:016x}", crate::obj::hash_value(version))[..10])
}

/// A duration as app.yaml writes it (`30ms`, `10m`, `1d`, `90`) in the API's (`600s`).
fn duration(key: &str, v: &Value) -> Result<Value, String> {
    let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    let s = s.trim();
    let (n, unit) = s.find(|c: char| c.is_ascii_alphabetic()).map_or((s, "s"), |i| (&s[..i], &s[i..]));
    let n: f64 = n.trim().parse().map_err(|_| format!("{key}: `{s}` isn't a duration (30ms, 10s, 10m, 1h, 1d)"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86400.0,
        _ => return Err(format!("{key}: `{s}` isn't a duration (30ms, 10s, 10m, 1h, 1d)")),
    };
    Ok(json!(if secs.fract() == 0.0 { format!("{}s", secs as i64) } else { format!("{secs:.3}s") }))
}

fn camel(key: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in key.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A map's keys in camelCase, all the way down; `keep` names maps whose keys are a person's own
/// (environment variables) and stay as they are.
fn camel_all(v: &Value, keep: &[&str]) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (camel(k), if keep.contains(&k.as_str()) { x.clone() } else { camel_all(x, keep) })).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|x| camel_all(x, keep)).collect()),
        other => other.clone(),
    }
}

fn upper(s: &str) -> String {
    s.to_ascii_uppercase().replace('-', "_")
}

/// One of app.yaml's handlers as the API's UrlMap.
fn handler(h: &Value) -> Result<Value, String> {
    let Value::Object(h) = h else { return Err("handlers: a list of handlers".into()) };
    let s = |k: &str| h.get(k).and_then(Value::as_str).map(str::to_owned);
    let url = s("url").ok_or("handlers: each has a url")?;
    let mut out = Map::new();
    let mut files = Map::new();
    if let Some(dir) = s("static_dir") {
        let dir = dir.trim_end_matches('/');
        out.insert("urlRegex".into(), json!(format!("{}/(.*)", url.trim_end_matches('/'))));
        files.insert("path".into(), json!(format!("{dir}/\\1")));
        files.insert("uploadPathRegex".into(), json!(format!("{dir}/.*")));
    } else {
        out.insert("urlRegex".into(), json!(url));
    }
    if let Some(f) = s("static_files") {
        files.insert("path".into(), json!(f));
        files.insert("uploadPathRegex".into(), json!(s("upload").unwrap_or_default()));
    }
    for (k, to) in [("http_headers", "httpHeaders"), ("mime_type", "mimeType"), ("require_matching_file", "requireMatchingFile"), ("application_readable", "applicationReadable")] {
        if let Some(v) = h.get(k) {
            files.insert(to.into(), v.clone());
        }
    }
    if let Some(e) = h.get("expiration") {
        files.insert("expiration".into(), duration("expiration", e)?);
    }
    if !files.is_empty() {
        out.insert("staticFiles".into(), Value::Object(files));
    }
    if let Some(sc) = s("script") {
        out.insert("script".into(), json!({ "scriptPath": sc }));
    }
    if let Some(v) = s("secure") {
        out.insert("securityLevel".into(), json!(format!("SECURE_{}", upper(&v))));
    }
    if let Some(v) = s("login") {
        out.insert("login".into(), json!(format!("LOGIN_{}", upper(&v))));
    }
    if let Some(v) = s("auth_fail_action") {
        out.insert("authFailAction".into(), json!(format!("AUTH_FAIL_ACTION_{}", upper(&v))));
    }
    if let Some(v) = h.get("redirect_http_response_code") {
        out.insert("redirectHttpResponseCode".into(), json!(format!("REDIRECT_HTTP_RESPONSE_CODE_{}", v.as_str().map_or_else(|| v.to_string(), str::to_owned))));
    }
    Ok(Value::Object(out))
}

/// app.yaml as the Admin API's Version.
pub fn from_app_yaml(v: &Value) -> Result<Value, String> {
    let Value::Object(m) = v else { return Err("app.yaml: a map of settings".into()) };
    let mut out = Map::new();
    for (k, x) in m {
        match k.as_str() {
            // The service is kiln's `service:`; the rest are the old runtimes' or gcloud's own.
            "service" | "module" | "application" | "version" | "skip_files" | "includes" | "api_version" | "builtins" | "derived_file_type" => {}
            "env" => {
                out.insert("env".into(), json!(if x.as_str() == Some("flex") { "flexible" } else { x.as_str().unwrap_or("standard") }));
            }
            "entrypoint" => {
                out.insert("entrypoint".into(), json!({ "shell": x }));
            }
            "env_variables" => {
                out.insert("envVariables".into(), x.clone());
            }
            "beta_settings" => {
                out.insert("betaSettings".into(), x.clone());
            }
            "default_expiration" => {
                out.insert("defaultExpiration".into(), duration(k, x)?);
            }
            "handlers" => {
                let list = x.as_array().ok_or("handlers: a list")?.iter().map(handler).collect::<Result<Vec<_>, _>>()?;
                out.insert("handlers".into(), Value::Array(list));
            }
            "inbound_services" => {
                let list: Vec<Value> = x.as_array().ok_or("inbound_services: a list")?.iter().filter_map(Value::as_str).map(|s| json!(format!("INBOUND_SERVICE_{}", upper(s)))).collect();
                out.insert("inboundServices".into(), Value::Array(list));
            }
            "vpc_access_connector" => {
                let mut c = Map::new();
                c.insert("name".into(), x.get("name").cloned().unwrap_or(Value::Null));
                if let Some(e) = x.get("egress_setting").and_then(Value::as_str) {
                    c.insert("egressSetting".into(), json!(if e.contains("all") { "ALL_TRAFFIC" } else { "PRIVATE_IP_RANGES" }));
                }
                out.insert("vpcAccessConnector".into(), Value::Object(c));
            }
            "automatic_scaling" => {
                let Value::Object(a) = x else { return Err("automatic_scaling: a map".into()) };
                let (mut scaling, mut scheduler) = (Map::new(), Map::new());
                for (sk, sv) in a {
                    match sk.as_str() {
                        "min_instances" | "max_instances" | "target_cpu_utilization" | "target_throughput_utilization" => {
                            scheduler.insert(camel(sk), sv.clone());
                        }
                        "min_pending_latency" | "max_pending_latency" if sv.as_str() == Some("automatic") => {}
                        "min_pending_latency" | "max_pending_latency" | "cool_down_period_sec" => {
                            scaling.insert(camel(sk.trim_end_matches("_sec")), duration(sk, sv)?);
                        }
                        "min_num_instances" | "max_num_instances" => {
                            scaling.insert(if sk.starts_with("min") { "minTotalInstances" } else { "maxTotalInstances" }.into(), sv.clone());
                        }
                        _ => {
                            scaling.insert(camel(sk), camel_all(sv, &[]));
                        }
                    }
                }
                if !scheduler.is_empty() {
                    scaling.insert("standardSchedulerSettings".into(), Value::Object(scheduler));
                }
                out.insert("automaticScaling".into(), Value::Object(scaling));
            }
            "basic_scaling" => {
                let mut b = Map::new();
                for (sk, sv) in x.as_object().ok_or("basic_scaling: a map")? {
                    b.insert(camel(sk), if sk == "idle_timeout" { duration(sk, sv)? } else { sv.clone() });
                }
                out.insert("basicScaling".into(), Value::Object(b));
            }
            _ => {
                out.insert(camel(k), camel_all(x, &[]));
            }
        }
    }
    Ok(Value::Object(out))
}

/// What a version deploys, as `source:` or `image:` says: `{zip: {artifact}}` for a zip CI built,
/// `{zip: {sourceUrl}}` for one in Cloud Storage, `{container: {image}}`.
fn deployment(spec: &Value) -> Result<Value, String> {
    match (text(spec, "source"), text(spec, "image")) {
        (Some(_), Some(_)) => Err("gcp-appengine takes source (a zip) or image (the flexible environment's), not both".into()),
        (Some(s), None) => {
            if let Some(hex) = s.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
                let mut zip = json!({ "artifact": format!("sha256:{}", hex.to_ascii_lowercase()) });
                if let (Some(b), Value::Object(m)) = (text(spec, "bucket"), &mut zip) {
                    m.insert("bucket".into(), json!(b));
                }
                return Ok(json!({ "zip": zip }));
            }
            if let Some((bucket, object)) = s.strip_prefix("gs://").and_then(|r| r.split_once('/')).filter(|(b, o)| !b.is_empty() && !o.is_empty()) {
                return Ok(json!({ "zip": { "sourceUrl": format!("https://storage.googleapis.com/{bucket}/{object}") } }));
            }
            Err(format!("source `{s}`: a zip CI built (${{{{ release.artifacts.<name>.digest }}}}), or gs://bucket/object.zip"))
        }
        (None, Some(i)) => Ok(json!({ "container": { "image": i } })),
        (None, None) => Err("gcp-appengine needs source: the zip CI built (${{ release.artifacts.<name>.digest }}), or image: for the flexible environment".into()),
    }
}

fn wave(mut o: Value, w: i64) -> Value {
    if let Some(ann) = crate::obj::ensure_map(&mut o, &["metadata", "annotations"]) {
        ann.insert(crate::sync::KILN_WAVE.into(), Value::String(w.to_string()));
    }
    o
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let service = text(input.spec, "service").ok_or("gcp-appengine needs service: the service (`default` for the app's first)")?;
    if !valid_name(service, 63) {
        return Err(format!("service `{service}`: up to 63 lowercase letters, digits and -, starting with a letter"));
    }
    let mut version = match file_or_object(input, "app")? {
        Some(v) if input.spec.get("app").is_some_and(Value::is_string) => from_app_yaml(&v)?,
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err("app: app.yaml, or the Version as an object".into()),
        None => json!({}),
    };
    let m = version.as_object_mut().ok_or("app: a map of settings")?;
    for k in OUTPUT {
        m.remove(*k);
    }
    m.insert("deployment".into(), deployment(input.spec)?);
    apply_sets(&mut version, input.spec.get("set"))?;
    if !version.get("runtime").and_then(Value::as_str).is_some_and(|r| !r.is_empty()) {
        return Err("gcp-appengine needs the version's runtime: app.yaml's runtime (python312, nodejs22, go122, …)".into());
    }
    let id = match text(input.spec, "version") {
        Some(v) if valid_name(v, 63) && !v.starts_with("ah-") && !matches!(v, "default" | "latest") => v.to_owned(),
        Some(v) => return Err(format!("version `{v}`: up to 63 lowercase letters, digits and -, not ah-…, default or latest")),
        None => version_id(&version),
    };
    let split = json!({ "split": { "allocations": { id.clone(): 1 } } });
    Ok(vec![wave(object(GROUP, "Version", &version_name(service, &id), None, version), 0), wave(object(GROUP, "Service", service, None, split), 1)])
}

// ------------------------------------------------------------------ App Engine's answers → objects

/// A Version object from App Engine's answer. One kiln made reads as kiln made it (`applied`):
/// a version doesn't change once made.
pub fn live_version(service: &str, answer: &Value, applied: Option<&Value>) -> Option<Value> {
    let id = answer.get("id")?.as_str()?;
    let spec = match applied.and_then(|a| a.get("spec")) {
        Some(s) => s.clone(),
        None => {
            let mut m = answer.as_object()?.clone();
            for k in OUTPUT.iter().filter(|k| **k != "deployment") {
                m.remove(*k);
            }
            Value::Object(m)
        }
    };
    let mut o = object(GROUP, "Version", &version_name(service, id), None, spec);
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "servingStatus": answer.get("servingStatus"), "versionUrl": answer.get("versionUrl"), "createTime": answer.get("createTime"), "createdBy": answer.get("createdBy") }));
    }
    Some(o)
}

/// A Service object from App Engine's answer: its traffic split.
pub fn live_service(answer: &Value) -> Option<Value> {
    let id = answer.get("id")?.as_str()?;
    let mut split = Map::new();
    if let Some(a) = answer.pointer("/split/allocations") {
        split.insert("allocations".into(), a.clone());
    }
    if let Some(s) = answer.pointer("/split/shardBy") {
        split.insert("shardBy".into(), s.clone());
    }
    let mut o = object(GROUP, "Service", id, None, json!({ "split": split }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "labels": answer.get("labels"), "networkSettings": answer.get("networkSettings") }));
    }
    Some(o)
}

/// The Version CreateVersion makes, deploying from `source_url` for a zip kiln put in Cloud
/// Storage.
pub fn create_body(desired: &Value, id: &str, source_url: Option<&str>) -> Value {
    let mut body = desired.get("spec").and_then(Value::as_object).cloned().unwrap_or_default();
    body.insert("id".into(), json!(id));
    if let (Some(url), Some(Value::Object(d))) = (source_url, body.get_mut("deployment")) {
        d.insert("zip".into(), json!({ "sourceUrl": url }));
    }
    Value::Object(body)
}

pub fn health(o: &Value) -> Option<Status> {
    match crate::obj::kind(o) {
        "Version" => match o.pointer("/status/servingStatus").and_then(Value::as_str).unwrap_or_default() {
            "SERVING" => status(Health::Healthy, ""),
            "STOPPED" => status(Health::Suspended, "stopped"),
            _ => status(Health::Progressing, "waiting for App Engine"),
        },
        "Service" => match o.pointer("/spec/split/allocations").and_then(Value::as_object) {
            Some(a) if !a.is_empty() => status(Health::Healthy, ""),
            _ => status(Health::Degraded, "no version takes the service's traffic"),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// app.yaml reads as the Admin API's Version; the same content is the same version, and the
    /// service's traffic all goes to it.
    #[test]
    fn app_yamls_read_as_versions() -> Result<(), String> {
        let yaml = "runtime: python312\ninstance_class: F2\nentrypoint: gunicorn -b :$PORT main:app\nautomatic_scaling:\n  max_instances: 4\n  target_cpu_utilization: 0.7\n  min_pending_latency: 30ms\nhandlers:\n- url: /static\n  static_dir: static\n  expiration: 1d\n- url: /.*\n  script: auto\n  secure: always\nenv_variables:\n  LOG_LEVEL: info\ninbound_services: [warmup]\n";
        let read = |p: &str| (p == "app.yaml").then(|| yaml.as_bytes().to_vec());
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let hex = "cd".repeat(32);
        let spec = json!({ "service": "web", "app": "app.yaml", "source": format!("sha256:{hex}"), "set": { "envVariables.MODE": "prod" } });
        let input = RenderInput { spec: &spec, target: &EMPTY, app: "default/web-staging", namespace: "", revision: "c0ffee", read: &read };
        let objs = render(&input)?;
        let (v, s) = (objs.first().ok_or("no version")?, objs.get(1).ok_or("no service")?);
        let spec = v.get("spec").ok_or("no spec")?;
        assert_eq!((spec.get("runtime"), spec.get("instanceClass")), (Some(&json!("python312")), Some(&json!("F2"))));
        assert_eq!(spec.get("entrypoint"), Some(&json!({ "shell": "gunicorn -b :$PORT main:app" })));
        assert_eq!(spec.pointer("/automaticScaling/standardSchedulerSettings"), Some(&json!({ "maxInstances": 4, "targetCpuUtilization": 0.7 })));
        assert_eq!(spec.pointer("/automaticScaling/minPendingLatency"), Some(&json!("0.030s")));
        assert_eq!(spec.pointer("/handlers/0"), Some(&json!({ "urlRegex": "/static/(.*)", "staticFiles": { "path": "static/\\1", "uploadPathRegex": "static/.*", "expiration": "86400s" } })));
        assert_eq!(spec.pointer("/handlers/1"), Some(&json!({ "urlRegex": "/.*", "script": { "scriptPath": "auto" }, "securityLevel": "SECURE_ALWAYS" })));
        assert_eq!(spec.get("envVariables"), Some(&json!({ "LOG_LEVEL": "info", "MODE": "prod" })), "a person's own names stay");
        assert_eq!(spec.get("inboundServices"), Some(&json!(["INBOUND_SERVICE_WARMUP"])));
        assert_eq!(spec.get("deployment"), Some(&json!({ "zip": { "artifact": format!("sha256:{hex}") } })));
        let (service, id) = split_name(crate::obj::name(v)).ok_or("version name")?;
        assert!(service == "web" && id.starts_with("kiln-"), "{id}");
        assert_eq!(s.pointer("/spec/split/allocations"), Some(&json!({ id: 1 })));
        assert!(crate::sync::wave_of(v) < crate::sync::wave_of(s), "the version before the traffic");
        Ok(())
    }
}
