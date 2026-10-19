//! What runs for a registration, as its provider sees it (docs/deploy-api.md §8.1): its resources,
//! the manifests it renders, what a sync would change, and its logs (txst's resources, render,
//! diff and logs), from KilnCD or Argo CD alike, in KilnCD's shapes. On Argo CD a `version`
//! previews another one: the manifests it renders, and what deploying it would change.

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
        None => kilncd_read(d, &kilncd::app_id(app, &env.name), what).await,
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
            return Err((StatusCode::NOT_IMPLEMENTED, "previewing another version on KilnCD isn't built yet: it shows what runs, and what git would change".into()));
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
            Ok(json!({ "changes": changes }))
        }
        Read::Diff { version: Some(v) } => {
            // What `v` renders, against what the deployed version renders: the chart's own change,
            // without the fields a cluster fills in.
            let keyed = |objects: Vec<Value>| -> BTreeMap<String, (Value, Value)> { objects.into_iter().map(|o| (key_text(&key_of_object(&o)), (key_of_object(&o), strip(&masked(o))))).collect() };
            let now = keyed(argo.manifests(name, None).await.map_err(unavailable)?);
            let then = keyed(argo.manifests(name, Some(&v)).await.map_err(unavailable)?);
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
            Ok(json!({ "version": v, "changes": changes }))
        }
        Read::Logs { group, kind, name: object, namespace, container, tail } => {
            let group = group.unwrap_or_else(|| group_of(&kind).to_owned());
            let lines = argo.logs(name, &group, &kind, &object, namespace.as_deref(), container.as_deref(), tail).await.map_err(unavailable)?;
            Ok(json!({ "lines": lines }))
        }
    }
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
}
