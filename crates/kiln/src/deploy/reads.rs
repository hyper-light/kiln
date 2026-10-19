//! What runs for a registration, as its provider sees it (docs/deploy-api.md §8.1): its resources,
//! the manifests it renders, what a sync would change, and its logs (txst's resources, render,
//! diff and logs), from KilnCD or Argo CD alike, in KilnCD's shapes. A `version` previews another
//! one: the manifests it renders, and what deploying it would change against what the deployed
//! one renders (Argo CD renders it; KilnCD renders what runs at that version).

use std::collections::BTreeMap;
use std::sync::Arc;

use http::StatusCode;
use serde_json::{Value, json};

use super::flow::Fail;
use super::model::{App, Environment};
use super::{Deploy, argocd, kilncd};

/// What is read.
pub enum Read {
    Resources,
    Manifests { version: Option<String> },
    Diff { version: Option<String> },
    /// A pod's log, or a workload's pods' (`kind`, `name`), the last `tail` lines.
    Logs { group: Option<String>, kind: String, name: String, namespace: Option<String>, container: Option<String>, tail: u32 },
}

fn unavailable(e: impl std::fmt::Display) -> Fail {
    (StatusCode::BAD_GATEWAY, e.to_string())
}

/// A registration's `what`, from its environment's provider.
pub async fn read(d: &Deploy, app: &App, env: &Environment, what: Read) -> Result<Value, Fail> {
    match env.provider.strip_prefix("argocd/") {
        Some(integration) => {
            let argo = argocd::Argo::of(&d.hub, integration).await.map_err(unavailable)?;
            argo_read(&argo, &argocd::application_name(app, &env.name), what).await
        }
        None => match what {
            Read::Manifests { version: Some(v) } => {
                let (_, then) = super::flow::kilncd_preview(d, app, env, &v).await?;
                Ok(json!({ "version": v, "manifests": then.into_iter().filter(synced).map(|o| json!({ "key": key_of_object(&o), "yaml": yaml(&masked(o)) })).collect::<Vec<_>>() }))
            }
            Read::Diff { version: Some(v) } => {
                let (now, then) = super::flow::kilncd_preview(d, app, env, &v).await?;
                Ok(json!({ "version": v, "diffs": compare(now, then) }))
            }
            what => kilncd_read(d, &kilncd::app_id(app, &env.name), what).await,
        },
    }
}

async fn kilncd_read(d: &Deploy, id: &str, what: Read) -> Result<Value, Fail> {
    let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running".to_owned()))?;
    let cx = crate::cd::ops::Cx { hub: Arc::clone(&d.hub), cd, actor: "deploy".into() };
    let (op, input) = match what {
        Read::Resources => ("cd.apps.resources", json!({ "app": id })),
        Read::Manifests { version: None } => ("cd.apps.manifests", json!({ "app": id })),
        Read::Diff { version: None } => ("cd.apps.diff", json!({ "app": id })),
        Read::Manifests { version: Some(_) } | Read::Diff { version: Some(_) } => {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "a version is previewed by read(), not here".into()));
        }
        Read::Logs { group, kind, name, namespace, container, tail } => {
            let group = group.unwrap_or_else(|| group_of(&kind).to_owned());
            ("cd.apps.resource_logs", json!({ "app": id, "group": group, "kind": kind, "name": name, "namespace": namespace.unwrap_or_default(), "container": container, "tail_lines": tail }))
        }
    };
    crate::cd::ops::call(cx, op, input).await
}

async fn argo_read(argo: &argocd::Argo, name: &str, what: Read) -> Result<Value, Fail> {
    match what {
        Read::Resources => {
            let app = argo.application(name, None).await.map_err(unavailable)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("Argo CD has no Application {name}")))?;
            let resources: Vec<Value> = app
                .pointer("/status/resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|r| {
                    json!({
                        "group": r.get("group").and_then(Value::as_str).unwrap_or(""),
                        "kind": r.get("kind"),
                        "namespace": r.get("namespace").and_then(Value::as_str).unwrap_or(""),
                        "name": r.get("name"),
                        "sync": r.get("status"),
                        "health": r.pointer("/health/status"),
                        "message": r.pointer("/health/message"),
                        "requires_pruning": r.get("requiresPruning").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect();
            Ok(json!({ "resources": resources }))
        }
        Read::Manifests { version } => {
            let manifests = argo.manifests(name, version.as_deref()).await.map_err(unavailable)?;
            Ok(json!({ "manifests": manifests.iter().map(|o| json!({ "key": key_of_object(o), "yaml": yaml(&masked(o.clone())) })).collect::<Vec<_>>() }))
        }
        Read::Diff { version: None } => {
            let items = argo.managed_resources(name).await.map_err(unavailable)?;
            let changes: Vec<Value> = items
                .iter()
                .filter(|i| i.get("modified").and_then(Value::as_bool).unwrap_or(false) || i.get("liveState").and_then(Value::as_str).is_none_or(|s| s == "null"))
                .map(|i| {
                    let state = |k: &str| i.get(k).and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok()).filter(|v| !v.is_null()).map(masked);
                    let live = state("normalizedLiveState").or_else(|| state("liveState"));
                    let predicted = state("predictedLiveState").or_else(|| state("targetState"));
                    change(key_of(i), live.as_ref(), predicted.as_ref())
                })
                .collect();
            Ok(json!({ "diffs": changes }))
        }
        Read::Diff { version: Some(v) } => {
            let now = argo.manifests(name, None).await.map_err(unavailable)?;
            let then = argo.manifests(name, Some(&v)).await.map_err(unavailable)?;
            Ok(json!({ "version": v, "diffs": compare(now, then) }))
        }
        Read::Logs { group, kind, name: object, namespace, container, tail } => {
            let group = group.unwrap_or_else(|| group_of(&kind).to_owned());
            let lines = argo.logs(name, &group, &kind, &object, namespace.as_deref(), container.as_deref(), tail).await.map_err(unavailable)?;
            Ok(json!({ "lines": lines }))
        }
    }
}

/// What another version renders (`then`) against what the deployed one renders (`now`), object by
/// object: the chart's own change, Secrets masked, without the fields a cluster fills in.
fn compare(now: Vec<Value>, then: Vec<Value>) -> Vec<Value> {
    let keyed = |objects: Vec<Value>| -> BTreeMap<String, (Value, Value)> { objects.into_iter().filter(synced).map(|o| (key_text(&key_of_object(&o)), (key_of_object(&o), strip(&masked(o))))).collect() };
    let (now, then) = (keyed(now), keyed(then));
    let mut changes = Vec::new();
    for (kt, (key, after)) in &then {
        match now.get(kt) {
            Some((_, before)) if before == after => {}
            before => changes.push(change(key.clone(), before.map(|(_, b)| b), Some(after))),
        }
    }
    for (kt, (key, before)) in &now {
        if !then.contains_key(kt) {
            changes.push(change(key.clone(), Some(before), None));
        }
    }
    changes
}

/// Whether a sync applies `o`: Helm's test hooks are rendered (each time with new random names),
/// and never synced.
fn synced(o: &Value) -> bool {
    !o.pointer("/metadata/annotations/helm.sh~1hook").and_then(Value::as_str).is_some_and(|h| h.contains("test"))
}

/// The API group of a kind whose pods have logs, when none is given.
fn group_of(kind: &str) -> &'static str {
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" => "apps",
        "Job" | "CronJob" => "batch",
        _ => "",
    }
}

/// One object's change, as `cd.apps.diff` shows it.
fn change(key: Value, live: Option<&Value>, predicted: Option<&Value>) -> Value {
    let (live, predicted) = (live.map(yaml).unwrap_or_default(), predicted.map(yaml).unwrap_or_default());
    let hunks = crate::harness::diff::hunks(&live, &predicted, 3);
    json!({ "key": key, "live": live, "predicted": predicted, "hunks": hunks })
}

fn key_of(r: &Value) -> Value {
    json!({ "group": r.get("group").and_then(Value::as_str).unwrap_or(""), "kind": r.get("kind"), "namespace": r.get("namespace").and_then(Value::as_str).unwrap_or(""), "name": r.get("name") })
}

fn key_of_object(o: &Value) -> Value {
    let group = o.get("apiVersion").and_then(Value::as_str).and_then(|v| v.rsplit_once('/')).map_or("", |(g, _)| g);
    json!({ "group": group, "kind": o.get("kind"), "namespace": o.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(""), "name": o.pointer("/metadata/name") })
}

fn key_text(k: &Value) -> String {
    let s = |f: &str| k.get(f).and_then(Value::as_str).unwrap_or("").to_owned();
    format!("{}/{}/{}/{}", s("group"), s("kind"), s("namespace"), s("name"))
}

fn yaml(o: &Value) -> String {
    serde_yaml::to_string(o).unwrap_or_default()
}

/// A Secret's values hidden, as kiln shows Secrets everywhere.
fn masked(mut o: Value) -> Value {
    if o.get("kind").and_then(Value::as_str) == Some("Secret") {
        for field in ["data", "stringData"] {
            if let Some(m) = o.get_mut(field).and_then(Value::as_object_mut) {
                for v in m.values_mut() {
                    *v = json!("++++++++");
                }
            }
        }
    }
    o
}

/// An object without what the cluster fills in, to compare a rendering with what runs.
fn strip(o: &Value) -> Value {
    let mut o = o.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("status");
        if let Some(meta) = m.get_mut("metadata").and_then(Value::as_object_mut) {
            for k in ["uid", "resourceVersion", "generation", "creationTimestamp", "managedFields", "selfLink"] {
                meta.remove(k);
            }
            if let Some(a) = meta.get_mut("annotations").and_then(Value::as_object_mut) {
                a.retain(|k, _| !k.starts_with("kubectl.kubernetes.io/") && !k.starts_with("argocd.argoproj.io/") && k != "deployment.kubernetes.io/revision");
            }
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_show_and_what_the_cluster_fills_in_isnt_a_change() {
        let s = masked(json!({ "kind": "Secret", "data": { "password": "aHVudGVyMg==" } }));
        assert_eq!(s.pointer("/data/password"), Some(&json!("++++++++")));
        let live = json!({ "kind": "ConfigMap", "metadata": { "name": "a", "uid": "1", "resourceVersion": "9", "annotations": { "kubectl.kubernetes.io/last-applied-configuration": "{}" } }, "data": { "x": "1" } });
        let rendered = json!({ "kind": "ConfigMap", "metadata": { "name": "a", "annotations": {} }, "data": { "x": "1" } });
        assert_eq!(strip(&live), strip(&rendered));
        assert_eq!(key_text(&key_of_object(&json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "web", "namespace": "shop" } }))), "apps/Deployment/shop/web");
    }

    #[test]
    fn another_version_is_what_it_renders_against_what_runs_renders() {
        let deploy = |tag: &str| json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "web", "namespace": "shop" }, "spec": { "image": format!("web:{tag}") } });
        let test_pod = |name: &str| json!({ "apiVersion": "v1", "kind": "Pod", "metadata": { "name": name, "namespace": "shop", "annotations": { "helm.sh/hook": "test-success" } } });
        let config = json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "same", "namespace": "shop" }, "data": { "a": "1" } });
        let gone = json!({ "apiVersion": "v1", "kind": "Service", "metadata": { "name": "old", "namespace": "shop" } });
        let secret = json!({ "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "db", "namespace": "shop" }, "data": { "password": "c2VjcmV0" } });
        let now = vec![deploy("1.0"), test_pod("web-test-a1b2c"), config.clone(), gone];
        let then = vec![deploy("1.1"), test_pod("web-test-z9y8x"), config, secret];
        let changes = compare(now, then);
        let names: Vec<&str> = changes.iter().filter_map(|c| c.pointer("/key/name").and_then(Value::as_str)).collect();
        assert_eq!(names, ["db", "web", "old"], "the new Secret, the changed Deployment, the Service gone; no test hook, nothing unchanged");
        let web = changes.iter().find(|c| c.pointer("/key/name").and_then(Value::as_str) == Some("web"));
        let lines: Vec<&str> = web.and_then(|c| c.pointer("/hunks/0/lines")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        assert!(lines.contains(&"-  image: web:1.0") && lines.contains(&"+  image: web:1.1"), "{lines:?}");
        let db = changes.iter().find(|c| c.pointer("/key/name").and_then(Value::as_str) == Some("db"));
        let shown = db.and_then(|c| c.get("predicted")).and_then(Value::as_str).unwrap_or_default();
        assert!(shown.contains("++++++++") && !shown.contains("c2VjcmV0"), "a Secret's values never show: {shown}");
    }
}
