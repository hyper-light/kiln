//! Deploying kiln with kiln (docs/cd-targets.md §10): a sync that changes the kiln server's own
//! Deployment can't watch its own restart, so it hands the watching to a **guard**.
//!
//! 1. Before the change is applied, the server keeps a record of it (the store's `self-deploy`
//!    setting) and starts a Job running the *previous* kiln image: `kiln cd guard`.
//! 2. The guard reads the Deployment every few seconds. When the new version's pods are
//!    available, it says so and ends. When they aren't within the deadline (or the rollout says
//!    it can't progress), it puts the previous image back, says why in the annotation
//!    `kiln.dev/guard`, and ends.
//! 3. The server that starts afterwards (the new version, or the previous one again) finds the
//!    record and finishes it: a deployment that succeeded, or one that failed with the guard's
//!    reason. A failed one is remembered as attempted, so automation doesn't deploy it again.
//!
//! Only a change of image is guarded; it is what a deploy of kiln changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use http::Method;
use kiln_cd::apps::ArtifactRef;
use kiln_cd::engine::{Input, Trigger};
use kiln_cd::obj;
use kiln_cd::state::{Command, Deployment};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Cd;
use super::kube::Cluster;

/// The store setting that holds a self-deploy in flight.
pub const SETTING: &str = "self-deploy";
/// The annotation the guard leaves on the Deployment.
pub const ANNOTATION: &str = "kiln.dev/guard";
/// How long the new version has to become available.
pub const DEADLINE: Duration = Duration::from_secs(600);

/// A self-deploy in flight: what the next server needs to finish its record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub app: String,
    pub op: u64,
    pub revision: Option<String>,
    pub key: Option<u64>,
    pub content: Option<u64>,
    pub commits: Vec<Option<String>>,
    pub artifacts: Vec<ArtifactRef>,
    pub trigger: String,
    pub by: Option<String>,
    pub started_ms: u64,
    pub namespace: String,
    pub deployment: String,
    pub container: String,
    pub from: String,
    pub to: String,
    pub job: Option<String>,
}

/// Where a rollout of `expect` stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its pods are available.
    Ready,
    /// Not yet.
    Waiting,
    /// It can't progress.
    Failed(String),
    /// The Deployment no longer asks for `expect` (someone changed it since): not the guard's.
    Moved,
}

fn int(v: &Value, path: &[&str]) -> Option<i64> {
    obj::i64_at(v, path)
}

/// Judges a Deployment's rollout of `expect` in `container`.
pub fn judge(d: &Value, container: &str, expect: &str) -> Verdict {
    let image = obj::get(d, &["spec", "template", "spec", "containers"])
        .and_then(Value::as_array)
        .and_then(|cs| cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(container)))
        .and_then(|c| c.get("image"))
        .and_then(Value::as_str);
    if image != Some(expect) {
        return Verdict::Moved;
    }
    let deadline_hit = obj::get(d, &["status", "conditions"]).and_then(Value::as_array).is_some_and(|cs| {
        cs.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("Progressing") && c.get("reason").and_then(Value::as_str) == Some("ProgressDeadlineExceeded"))
    });
    if deadline_hit {
        return Verdict::Failed("its rollout passed its progress deadline".into());
    }
    let generation = int(d, &["metadata", "generation"]).unwrap_or(0);
    if int(d, &["status", "observedGeneration"]).unwrap_or(0) < generation {
        return Verdict::Waiting;
    }
    let want = int(d, &["spec", "replicas"]).unwrap_or(1);
    let updated = int(d, &["status", "updatedReplicas"]).unwrap_or(0);
    let available = int(d, &["status", "availableReplicas"]).unwrap_or(0);
    let unavailable = int(d, &["status", "unavailableReplicas"]).unwrap_or(0);
    if updated >= want && available >= want && unavailable == 0 { Verdict::Ready } else { Verdict::Waiting }
}

/// The guard's Job: the previous kiln image, as kiln's own service account, pulling as kiln's pod
/// does.
pub fn job(m: &Marker, template: &Value) -> Value {
    let pod = obj::get(template, &["spec"]).cloned().unwrap_or(Value::Null);
    let deadline = DEADLINE.as_secs();
    let mut spec = json!({
        "restartPolicy": "Never",
        "containers": [{
            "name": "guard",
            "image": m.from,
            "command": ["/kiln", "cd", "guard",
                "--namespace", m.namespace, "--deployment", m.deployment, "--container", m.container,
                "--previous", m.from, "--expect", m.to, "--deadline", deadline.to_string()],
            "resources": { "requests": { "cpu": "10m", "memory": "32Mi" }, "limits": { "memory": "128Mi" } },
        }],
    });
    if let Some(m) = spec.as_object_mut() {
        for k in ["serviceAccountName", "imagePullSecrets", "nodeSelector", "tolerations", "securityContext"] {
            if let Some(v) = pod.get(k) {
                m.insert(k.to_owned(), v.clone());
            }
        }
    }
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "generateName": format!("{}-guard-", m.deployment),
            "namespace": m.namespace,
            "labels": { "app.kubernetes.io/name": "kiln-guard", "app.kubernetes.io/part-of": "kiln" },
        },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": deadline + 300,
            "ttlSecondsAfterFinished": 86_400,
            "template": { "metadata": { "labels": { "app.kubernetes.io/name": "kiln-guard" } }, "spec": spec },
        },
    })
}

fn deployment_path(namespace: &str, name: &str) -> String {
    format!("/apis/apps/v1/namespaces/{}/deployments/{}", kiln_cd::kube::enc(namespace), kiln_cd::kube::enc(name))
}

async fn patch(c: &Cluster, namespace: &str, name: &str, body: &Value) -> Result<()> {
    let (status, bytes) = c.request(Method::PATCH, &deployment_path(namespace, name), Some("application/strategic-merge-patch+json"), Some(Bytes::from(body.to_string()))).await?;
    if !(200..300).contains(&status) {
        bail!("{}", kiln_cd::kube::status_message(status, &bytes));
    }
    Ok(())
}

/// What `kiln cd guard` is told.
#[derive(Debug, Clone)]
pub struct Args {
    pub namespace: String,
    pub deployment: String,
    pub container: String,
    pub previous: String,
    pub expect: String,
    pub deadline: Duration,
    /// From outside the cluster (by hand, or a test): a kubeconfig context, and its file.
    pub context: Option<(String, String)>,
}

/// `kiln cd guard`: watches the rollout of `expect`; puts `previous` back if it doesn't become
/// available in time. Runs in a pod, as kiln's service account.
pub async fn run(a: &Args) -> Result<()> {
    let ep = match &a.context {
        Some((context, kubeconfig)) => super::kubeconfig_endpoint(kubeconfig, context)?,
        None => kiln_cd::kube::in_cluster().context("the guard runs in a pod, as kiln's service account (or give it --context)")?,
    };
    let c = Cluster::new("guard", ep)?;
    let started = Instant::now();
    loop {
        let read = c.request(Method::GET, &deployment_path(&a.namespace, &a.deployment), None, None).await;
        match read {
            Ok((200, bytes)) => {
                let d: Value = serde_json::from_slice(&bytes)?;
                match judge(&d, &a.container, &a.expect) {
                    Verdict::Ready => {
                        println!("{} is available: the deploy of {} took", a.deployment, a.expect);
                        let _ = patch(&c, &a.namespace, &a.deployment, &json!({ "metadata": { "annotations": { ANNOTATION: note(a, "ready", "") } } })).await;
                        return Ok(());
                    }
                    Verdict::Moved => {
                        println!("{} no longer asks for {}: someone changed it since; nothing to guard", a.deployment, a.expect);
                        return Ok(());
                    }
                    Verdict::Failed(reason) => return roll_back(&c, a, &reason).await,
                    Verdict::Waiting => {}
                }
            }
            Ok((404, _)) => bail!("{}/{} is gone", a.namespace, a.deployment),
            Ok((status, bytes)) => eprintln!("reading {}: {}", a.deployment, kiln_cd::kube::status_message(status, &bytes)),
            Err(e) => eprintln!("reading {}: {e:#}", a.deployment),
        }
        if started.elapsed() >= a.deadline {
            let reason = format!("its pods weren't available within {} s", a.deadline.as_secs());
            return roll_back(&c, a, &reason).await;
        }
        crate::rt::sleep(Duration::from_secs(5)).await;
    }
}

/// What the guard leaves in its annotation.
fn note(a: &Args, result: &str, reason: &str) -> String {
    json!({ "result": result, "from": a.previous, "to": a.expect, "reason": reason, "at": crate::util::now_unix() }).to_string()
}

async fn roll_back(c: &Cluster, a: &Args, reason: &str) -> Result<()> {
    eprintln!("{} didn't take ({reason}): putting {} back", a.expect, a.previous);
    let body = json!({
        "metadata": { "annotations": { ANNOTATION: note(a, "rolled-back", reason) } },
        "spec": { "template": { "spec": { "containers": [{ "name": a.container, "image": a.previous }] } } },
    });
    patch(c, &a.namespace, &a.deployment, &body).await.context("putting the previous image back")?;
    bail!("{} didn't take ({reason}); {} is back", a.expect, a.previous)
}

/// The image of `container` in a pod template (or the first container's, with no name).
fn image_of(template: &Value, container: Option<&str>) -> Option<(String, String)> {
    let cs = obj::get(template, &["spec", "containers"])?.as_array()?;
    let c = match container {
        Some(n) => cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(n))?,
        None => cs.first()?,
    };
    Some((c.get("name")?.as_str()?.to_owned(), c.get("image")?.as_str()?.to_owned()))
}

impl Cd {
    /// Before kiln applies its own Deployment: if the image changes, keeps the record and starts
    /// the guard. Errors here stop the apply: an unguarded self-deploy is worse than none.
    pub(crate) async fn before_self_apply(self: &Arc<Self>, object: &Value, cluster: &Cluster) -> Result<()> {
        let Some((own, namespace)) = crate::updates::kube_identity() else { return Ok(()) };
        if obj::group(object) != "apps" || obj::kind(object) != "Deployment" || obj::name(object) != own || obj::namespace(object) != namespace {
            return Ok(());
        }
        let live = cluster.request(Method::GET, &deployment_path(&namespace, &own), None, None).await?;
        let live: Value = match live {
            (200, bytes) => serde_json::from_slice(&bytes)?,
            (status, bytes) => bail!("reading kiln's own Deployment: {}", kiln_cd::kube::status_message(status, &bytes)),
        };
        let template = obj::get(&live, &["spec", "template"]).cloned().unwrap_or(Value::Null);
        let desired = obj::get(object, &["spec", "template"]).cloned().unwrap_or(Value::Null);
        let Some((container, from)) = image_of(&template, None) else { return Ok(()) };
        let Some((_, to)) = image_of(&desired, Some(&container)) else { return Ok(()) };
        if from == to {
            return Ok(());
        }
        // A retry of the same apply keeps the guard it started.
        let have: Option<Marker> = self.store.read(|d| d.settings.get(SETTING).cloned()).and_then(|v| serde_json::from_value(v).ok());
        if have.as_ref().is_some_and(|m| m.to == to && m.from == from) {
            return Ok(());
        }
        let app = obj::annotation(object, "kiln.dev/tracking").and_then(|t| t.split_once(':').map(|(a, _)| a.to_owned())).unwrap_or_default();
        let started = self.started.lock().get(&app).cloned();
        let (key, content, commits, artifacts) = {
            let apps = self.apps.read();
            let e = apps.get(&app);
            let r = e.and_then(|e| e.key.and_then(|k| e.resolved.iter().find(|r| r.key == k)));
            (r.map(|r| r.key), r.map(|r| r.content), r.map(|r| r.commits.clone()).unwrap_or_default(), r.map(|r| r.artifacts.clone()).unwrap_or_default())
        };
        let mut m = Marker {
            app,
            op: started.as_ref().map_or(0, |s| s.op),
            revision: started.as_ref().and_then(|s| s.revision.clone()),
            key,
            content,
            commits,
            artifacts,
            trigger: started.as_ref().map_or_else(|| "manual".to_owned(), |s| s.trigger.clone()),
            by: started.as_ref().and_then(|s| s.by.clone()),
            started_ms: started.as_ref().map_or_else(super::now_ms, |s| s.at_ms),
            namespace: namespace.clone(),
            deployment: own,
            container,
            from,
            to,
            job: None,
        };
        let job = job(&m, &template);
        let path = format!("/apis/batch/v1/namespaces/{}/jobs", kiln_cd::kube::enc(&namespace));
        let (status, bytes) = cluster.request(Method::POST, &path, Some("application/json"), Some(Bytes::from(job.to_string()))).await?;
        if !(200..300).contains(&status) {
            bail!("starting the guard: {}", kiln_cd::kube::status_message(status, &bytes));
        }
        m.job = serde_json::from_slice::<Value>(&bytes).ok().and_then(|j| obj::get(&j, &["metadata", "name"]).and_then(Value::as_str).map(str::to_owned));
        let value = serde_json::to_value(&m)?;
        match self.store.submit(Command::PutSetting(SETTING.into(), value)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!("keeping the self-deploy's record: {e}"),
            Err(_) => bail!("the store stopped"),
        }
        tracing::info!(from = %m.from, to = %m.to, guard = ?m.job, "deployments: deploying kiln itself; a guard watches the restart");
        Ok(())
    }

    /// On a restart: finishes the record of a self-deploy that was in flight. The Deployment says
    /// how it went: the new image means it took; the previous one means the guard put it back.
    pub(crate) async fn finish_self_deploy(self: &Arc<Self>, cluster: &Cluster) {
        let Some(m) = self.store.read(|d| d.settings.get(SETTING).cloned()).filter(|v| !v.is_null()).and_then(|v| serde_json::from_value::<Marker>(v).ok()) else { return };
        let read = cluster.request(Method::GET, &deployment_path(&m.namespace, &m.deployment), None, None).await;
        let Ok((200, bytes)) = read else {
            tracing::warn!("deployments: can't read kiln's own Deployment to finish a self-deploy's record; trying at the next start");
            return;
        };
        let Ok(live) = serde_json::from_slice::<Value>(&bytes) else { return };
        let template = obj::get(&live, &["spec", "template"]).cloned().unwrap_or(Value::Null);
        let now = image_of(&template, Some(&m.container)).map(|(_, i)| i).unwrap_or_default();
        let guard: Option<Value> = obj::annotation(&live, ANNOTATION).and_then(|a| serde_json::from_str(a).ok());
        let (succeeded, message) = if now == m.to {
            (true, format!("kiln restarted into {}", m.to))
        } else if now == m.from {
            let why = guard.as_ref().filter(|g| g.get("to").and_then(Value::as_str) == Some(m.to.as_str())).and_then(|g| g.get("reason").and_then(Value::as_str)).unwrap_or("it didn't start");
            (false, format!("{} didn't take ({why}); the guard put {} back", m.to, m.from))
        } else {
            (false, format!("kiln's Deployment runs {now}, neither the image deployed nor the one before: someone changed it since"))
        };
        let trigger = match m.trigger.as_str() {
            "rollback" => Trigger::Rollback,
            "promotion" => Trigger::Promotion,
            "auto" => Trigger::Auto,
            "self-heal" => Trigger::SelfHeal,
            _ => Trigger::Manual,
        };
        let limit = self.apps.read().get(&m.app).map_or(10, |e| e.def.history_limit);
        let d = Deployment {
            app: m.app.clone(),
            op: m.op,
            revision: m.revision.clone(),
            key: m.key,
            trigger: trigger.as_str().to_owned(),
            by: m.by.clone(),
            succeeded,
            message: message.clone(),
            applied: 0,
            pruned: 0,
            at_ms: super::now_ms(),
            id: 0,
            commits: m.commits.clone(),
            content: m.content,
            artifacts: m.artifacts.clone(),
        };
        if !matches!(self.store.submit(Command::Deployed { deployment: Box::new(d), limit }).await, Ok(Ok(()))) {
            tracing::warn!("deployments: recording a self-deploy failed; trying at the next start");
            return;
        }
        let _ = self.store.submit(Command::PutSetting(SETTING.into(), Value::Null)).await;
        // A failed one counts as attempted: automation doesn't deploy the same content again.
        if let Some(content) = m.content {
            self.input(Input::Remember { app: m.app.as_str().into(), content, succeeded });
        }
        if succeeded {
            tracing::info!(app = %m.app, message = %message, "deployments: a self-deploy took");
        } else {
            tracing::warn!(app = %m.app, message = %message, "deployments: a self-deploy didn't take");
        }
    }
}

/// What a sync's start said, kept for a self-deploy's record.
#[derive(Clone, Debug)]
pub struct Started {
    pub op: u64,
    pub revision: Option<String>,
    pub trigger: String,
    pub by: Option<String>,
    pub at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(image: &str, generation: i64, observed: i64, updated: i64, available: i64, unavailable: i64) -> Value {
        json!({
            "metadata": { "generation": generation },
            "spec": { "replicas": 1, "template": { "spec": { "containers": [{ "name": "kiln", "image": image }] } } },
            "status": { "observedGeneration": observed, "updatedReplicas": updated, "availableReplicas": available, "unavailableReplicas": unavailable },
        })
    }

    #[test]
    fn the_guard_judges_a_rollout() {
        let new = "docker.io/acme/kiln@sha256:new";
        assert_eq!(judge(&deployment(new, 5, 5, 1, 1, 0), "kiln", new), Verdict::Ready);
        assert_eq!(judge(&deployment(new, 5, 4, 1, 1, 0), "kiln", new), Verdict::Waiting, "not observed yet");
        assert_eq!(judge(&deployment(new, 5, 5, 1, 0, 1), "kiln", new), Verdict::Waiting, "starting");
        assert_eq!(judge(&deployment("docker.io/acme/kiln@sha256:other", 6, 6, 1, 1, 0), "kiln", new), Verdict::Moved);
        let mut stuck = deployment(new, 5, 5, 1, 0, 1);
        if let Some(s) = stuck.get_mut("status") {
            s["conditions"] = json!([{ "type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded" }]);
        }
        assert!(matches!(judge(&stuck, "kiln", new), Verdict::Failed(_)));
    }

    #[test]
    fn the_guard_runs_the_previous_image_as_kiln() {
        let m = Marker { namespace: "kiln".into(), deployment: "kiln".into(), container: "kiln".into(), from: "acme/kiln@sha256:old".into(), to: "acme/kiln@sha256:new".into(), ..Marker::default() };
        let template = json!({ "spec": { "serviceAccountName": "kiln", "imagePullSecrets": [{ "name": "kiln-registry" }], "containers": [] } });
        let j = job(&m, &template);
        assert_eq!(j.pointer("/spec/template/spec/containers/0/image").and_then(Value::as_str), Some("acme/kiln@sha256:old"));
        assert_eq!(j.pointer("/spec/template/spec/serviceAccountName").and_then(Value::as_str), Some("kiln"));
        assert_eq!(j.pointer("/spec/template/spec/imagePullSecrets/0/name").and_then(Value::as_str), Some("kiln-registry"));
        let cmd: Vec<&str> = j.pointer("/spec/template/spec/containers/0/command").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        assert_eq!(cmd.get(..3), Some(&["/kiln", "cd", "guard"][..]));
        assert!(cmd.windows(2).any(|w| w == ["--expect", "acme/kiln@sha256:new"]));
        assert_eq!(j.pointer("/spec/backoffLimit").and_then(Value::as_i64), Some(0));
    }
}
