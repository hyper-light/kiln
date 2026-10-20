//! What can be done to an object a registration runs, through its provider (docs/deploy-api.md
//! §8.1's `action`): on Argo CD, Argo CD's own resource actions (its built-in ones, restart, pause,
//! resume, suspend, scale and the rest, and any it was given), and deleting a pod; on KilnCD, kiln's,
//! made as `kubectl` makes them: restart a workload, pause and resume a Deployment's rollout, scale
//! it, suspend and resume a CronJob or run it now, delete a pod for its workload to make again.

use std::collections::BTreeMap;

use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::flow::{Fail, Who};
use super::model::{App, Environment};
use super::{Deploy, argocd, store};

/// An object of a registration's, as an action names it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Its API group (`apps`); its kind's, when left out.
    #[serde(default)]
    pub group: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
}

/// An action asked for on an object.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    #[serde(flatten)]
    pub target: Target,
    pub action: String,
    /// What it takes (`replicas` for a scale).
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

fn unavailable(e: impl std::fmt::Display) -> Fail {
    (StatusCode::BAD_GATEWAY, e.to_string())
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

/// The API group of a kind, when none is given.
fn group_of(kind: &str) -> &'static str {
    match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" => "apps",
        "Job" | "CronJob" => "batch",
        "HorizontalPodAutoscaler" => "autoscaling",
        "Ingress" | "NetworkPolicy" => "networking.k8s.io",
        _ => "",
    }
}

/// The actions an object offers now, each `{name, disabled, params?}`.
pub async fn list(d: &Deploy, a: &App, env: &Environment, t: &Target) -> Result<Vec<Value>, Fail> {
    let group = t.group.clone().unwrap_or_else(|| group_of(&t.kind).to_owned());
    match env.provider.strip_prefix("argocd/") {
        Some(integration) => {
            let argo = argocd::Argo::of(&d.hub, integration).await.map_err(unavailable)?;
            let name = argocd::application_name(a, &env.name);
            let app = argo.application(&name, None).await.map_err(unavailable)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("Argo CD has no Application {name}")))?;
            let o = argocd::Object::of(&app, &group, &t.kind, t.namespace.as_deref().unwrap_or(""), &t.name);
            let mut actions: Vec<Value> = argo
                .actions(&name, &o)
                .await
                .map_err(unavailable)?
                .into_iter()
                .map(|x| json!({ "name": x.get("name"), "disabled": x.get("disabled").and_then(Value::as_bool).unwrap_or(false), "params": x.get("params") }))
                .collect();
            if t.kind == "Pod" {
                actions.push(json!({ "name": "delete", "disabled": false }));
            }
            Ok(actions)
        }
        None => {
            let live = live(d, a, env, &group, t).await?;
            Ok(kiln_actions(&group, &t.kind, &live))
        }
    }
}

/// Runs an action on an object: what it did, in a line.
pub async fn run(d: &Deploy, who: &Who, a: &App, env: &Environment, ask: &Ask) -> Result<Value, Fail> {
    if who.agent && !env.agents.deploy {
        return Err((StatusCode::FORBIDDEN, format!("agents may not act on what runs in {}: a person does, or the environment lets agents deploy", env.name)));
    }
    let t = &ask.target;
    let group = t.group.clone().unwrap_or_else(|| group_of(&t.kind).to_owned());
    let what = format!("{} {}/{} of {} in {}", ask.action, t.kind, t.name, a.name, env.name);
    match env.provider.strip_prefix("argocd/") {
        Some(integration) => {
            let argo = argocd::Argo::of(&d.hub, integration).await.map_err(unavailable)?;
            let name = argocd::application_name(a, &env.name);
            let app = argo.application(&name, None).await.map_err(unavailable)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("Argo CD has no Application {name}")))?;
            let o = argocd::Object::of(&app, &group, &t.kind, t.namespace.as_deref().unwrap_or(""), &t.name);
            if ask.action == "delete" {
                if t.kind != "Pod" {
                    return Err(bad("only a pod is deleted as an action: what else runs is the release's to remove"));
                }
                argo.delete_object(&name, &o).await.map_err(unavailable)?;
            } else {
                argo.run_action(&name, &o, &ask.action, &ask.params).await.map_err(unavailable)?;
            }
        }
        None => {
            let live = live(d, a, env, &group, t).await?;
            let offered = kiln_actions(&group, &t.kind, &live);
            let ok = offered.iter().any(|x| x.get("name").and_then(Value::as_str) == Some(ask.action.as_str()) && x.get("disabled").and_then(Value::as_bool) != Some(true));
            if !ok {
                let names: Vec<&str> = offered.iter().filter(|x| x.get("disabled").and_then(Value::as_bool) != Some(true)).filter_map(|x| x.get("name").and_then(Value::as_str)).collect();
                return Err(bad(format!("{} {} offers {}", t.kind, t.name, if names.is_empty() { "no action now".to_owned() } else { names.join(", ") })));
            }
            kiln_run(env, &group, t, &live, &ask.action, &ask.params).await?;
        }
    }
    d.hub.store.audit(who.name.clone(), "deploy.action".into(), what.clone()).await;
    Ok(json!({ "done": what }))
}

/// The cluster a KilnCD environment runs in, as KilnCD reaches it.
fn cluster(env: &Environment) -> Result<std::sync::Arc<crate::cd::kube::Cluster>, Fail> {
    let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running".to_owned()))?;
    let name = env.targets.kubernetes.first().cloned().unwrap_or_else(|| kiln_cd::apps::IN_CLUSTER.to_owned());
    cd.clusters.read().get(&name).cloned().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, format!("KilnCD has no cluster {name}")))
}

/// The object as it runs, once it's one the registration runs: in its namespace, and (but for a
/// pod, which its workloads make) one of KilnCD's app's.
async fn live(d: &Deploy, a: &App, env: &Environment, group: &str, t: &Target) -> Result<Value, Fail> {
    let reg = store::registration(&d.hub.store, &a.name, &env.name).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if reg.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("{} isn't registered in {}", a.name, env.name)));
    }
    let namespace = t.namespace.clone().unwrap_or_else(|| a.namespace().to_owned());
    if t.kind != "Pod" {
        let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running".to_owned()))?;
        let (id, g, k, n) = (super::kilncd::app_id(a, &env.name), group.to_owned(), t.kind.clone(), t.name.clone());
        let ours = cd.query(move |e| e.resources(&id).iter().any(|r| r.key.group.as_ref() == g && r.key.kind.as_ref() == k && r.key.name.as_ref() == n)).await.unwrap_or(false);
        if !ours {
            return Err((StatusCode::NOT_FOUND, format!("{} doesn't run {} {} in {}", a.name, t.kind, t.name, env.name)));
        }
    }
    let key = kiln_cd::obj::ResKey::new(group, &t.kind, &namespace, &t.name);
    cluster(env)?.get(&key).await.map_err(unavailable)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{} {} isn't in {namespace}", t.kind, t.name)))
}

/// The actions kiln offers on an object of a KilnCD app, as it is now.
fn kiln_actions(group: &str, kind: &str, live: &Value) -> Vec<Value> {
    let flag = |p: &str| live.pointer(p).and_then(Value::as_bool).unwrap_or(false);
    let action = |name: &str, disabled: bool| json!({ "name": name, "disabled": disabled });
    match (group, kind) {
        ("apps", "Deployment") => {
            let paused = flag("/spec/paused");
            vec![action("restart", false), action("pause", paused), action("resume", !paused), json!({ "name": "scale", "disabled": false, "params": [{ "name": "replicas" }] })]
        }
        ("apps", "StatefulSet") => vec![action("restart", false), json!({ "name": "scale", "disabled": false, "params": [{ "name": "replicas" }] })],
        ("apps", "DaemonSet") => vec![action("restart", false)],
        ("batch", "CronJob") => {
            let suspended = flag("/spec/suspend");
            vec![action("create-job", false), action("suspend", suspended), action("resume", !suspended)]
        }
        ("", "Pod") => vec![action("delete", false)],
        _ => Vec::new(),
    }
}

/// The field manager kiln's actions change objects as.
const MANAGER: &str = "kiln-action";

async fn kiln_run(env: &Environment, group: &str, t: &Target, live: &Value, action: &str, params: &BTreeMap<String, String>) -> Result<(), Fail> {
    let cluster = cluster(env)?;
    let namespace = live.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("").to_owned();
    let key = kiln_cd::obj::ResKey::new(group, &t.kind, &namespace, &t.name);
    let now = super::repo::rfc3339(crate::util::now_unix());
    let patch = match (t.kind.as_str(), action) {
        (_, "restart") => json!({ "spec": { "template": { "metadata": { "annotations": { "kubectl.kubernetes.io/restartedAt": now } } } } }),
        ("Deployment", "pause") => json!({ "spec": { "paused": true } }),
        ("Deployment", "resume") => json!({ "spec": { "paused": null } }),
        ("CronJob", "suspend") => json!({ "spec": { "suspend": true } }),
        ("CronJob", "resume") => json!({ "spec": { "suspend": false } }),
        (_, "scale") => {
            let replicas: u32 = params.get("replicas").and_then(|r| r.trim().parse().ok()).ok_or_else(|| bad("a scale takes replicas: a whole number"))?;
            json!({ "spec": { "replicas": replicas } })
        }
        ("Pod", "delete") => {
            cluster.delete(&key, "Background").await.map_err(unavailable)?;
            return Ok(());
        }
        ("CronJob", "create-job") => {
            let template = live.pointer("/spec/jobTemplate").cloned().ok_or_else(|| bad(format!("CronJob {} has no job template", t.name)))?;
            let mut job = json!({
                "apiVersion": "batch/v1",
                "kind": "Job",
                "metadata": {
                    "name": format!("{}-{}", t.name.chars().take(40).collect::<String>(), crate::util::now_unix()),
                    "namespace": namespace,
                    "annotations": { "cronjob.kubernetes.io/instantiate": "manual" },
                    "ownerReferences": [{ "apiVersion": "batch/v1", "kind": "CronJob", "name": t.name, "uid": live.pointer("/metadata/uid"), "controller": true }],
                },
                "spec": template.get("spec").cloned().unwrap_or(Value::Null),
            });
            if let (Some(labels), Some(meta)) = (template.pointer("/metadata/labels").cloned(), job.get_mut("metadata").and_then(Value::as_object_mut)) {
                meta.insert("labels".into(), labels);
            }
            cluster.create(&job, MANAGER).await.map_err(unavailable)?;
            return Ok(());
        }
        _ => return Err(bad(format!("{} doesn't {action}", t.kind))),
    };
    cluster.merge_patch(&key, &patch, MANAGER).await.map_err(unavailable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiln_offers_what_an_object_can_do_now() {
        let names = |v: Vec<Value>| v.iter().filter(|x| x.get("disabled") != Some(&json!(true))).filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_owned)).collect::<Vec<_>>();
        assert_eq!(names(kiln_actions("apps", "Deployment", &json!({ "spec": {} }))), ["restart", "pause", "scale"], "running: it can't resume");
        assert_eq!(names(kiln_actions("apps", "Deployment", &json!({ "spec": { "paused": true } }))), ["restart", "resume", "scale"]);
        assert_eq!(names(kiln_actions("batch", "CronJob", &json!({ "spec": { "suspend": true } }))), ["create-job", "resume"]);
        assert_eq!(names(kiln_actions("", "Pod", &json!({}))), ["delete"]);
        assert!(kiln_actions("", "Service", &json!({})).is_empty());
        assert_eq!(group_of("CronJob"), "batch");
    }

    #[test]
    fn an_action_names_its_object_and_what_it_takes() -> Result<(), String> {
        let ask: Ask = serde_json::from_value(json!({ "kind": "Deployment", "name": "web", "action": "scale", "params": { "replicas": "3" } })).map_err(|e| e.to_string())?;
        assert_eq!((ask.target.kind.as_str(), ask.action.as_str(), ask.params.get("replicas").map(String::as_str)), ("Deployment", "scale", Some("3")));
        assert!(ask.target.group.is_none() && ask.target.namespace.is_none());
        Ok(())
    }
}
