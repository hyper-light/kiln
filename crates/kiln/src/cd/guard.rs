//! Deploying kiln with kiln (docs/cd-targets.md §10): a sync that changes the kiln server's own
//! Deployment can't watch its own restart, so it hands the watching to a **guard**.
//!
//! 1. Before the change is applied, the server keeps a record of it (the store's `self-deploy`
//!    setting) and starts a Job running the *previous* kiln image: `kiln cd guard`.
//! 2. The guard reads the Deployment every few seconds until the new version's pods are
//!    available. When they aren't within the deadline, the rollout says it can't progress, or
//!    their containers keep restarting, it puts the previous image back.
//! 3. Then it watches the new version for a window: the app's `rollout: all-at-once: { after,
//!    window }`, or five minutes with no checks of its own when the app says none. Its pods must
//!    stay available, none of their containers may restart, kiln's server in them must not be
//!    started again by its supervisor (after a crash or the kernel's OOM killer, which a pod's
//!    status doesn't show), and the app's after checks must pass. Anything failing puts the
//!    previous image back. The annotation `kiln.dev/guard` says how the watch goes, then how it
//!    ended and why.
//! 4. The server that runs afterwards (the new version, or the previous one again) finishes the
//!    record once the guard has said: a deployment that succeeded, or one that failed with the
//!    guard's reason. A failed one is remembered as attempted, so automation doesn't deploy it
//!    again.
//!
//! Only a change of image is guarded; it is what a deploy of kiln changes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use http::Method;
use kiln_cd::apps::{AppDef, ArtifactRef};
use kiln_cd::engine::{Input, Trigger};
use kiln_cd::obj;
use kiln_cd::rollout::{CheckSpec, Measurement, Watched, Window};
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
/// How long the new version is watched once its pods are up, when its app says nothing.
pub const WINDOW: Duration = Duration::from_secs(300);
/// How long an all-at-once rollout's after checks watch when it gives no window.
const AFTER_WINDOW: Duration = Duration::from_secs(600);
/// How often the guard reads the Deployment and its pods.
const LOOK: Duration = Duration::from_secs(5);
/// Restarts of the new version's containers, before its pods are up, that mean it can't start.
const CRASH_LOOP: i64 = 3;
/// The guard starts before the change is applied: for this long, a Deployment still asking for
/// the previous image hasn't been changed yet, rather than changed back.
const APPLY_GRACE: Duration = Duration::from_secs(120);
/// Reads in a row that find the new pods unavailable while watching, before that fails it.
const UNAVAILABLE_READS: u32 = 3;
/// How often the annotation says how the watch goes.
const SAY: Duration = Duration::from_secs(30);
/// kiln's own report of itself (`/healthz/server`), on its pod, and how long it may take.
const SERVER_HEALTH: &str = "/healthz/server";
const SERVER_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// What the guard watches once the new version's pods are up (docs/cd-targets.md §10).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    /// How long, in seconds; 0: not at all (the record of a guard from before windows).
    pub window_s: u64,
    /// The app's after checks, in order, and each one's spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, CheckSpec>,
    /// What the checks read as `rollout.*`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub rollout: Value,
}

impl Watch {
    /// What an app's rollout asks of a deploy of kiln itself: an all-at-once rollout's after
    /// checks, for its window; with no rollout, the default window and no checks of its own.
    /// Nothing else fits: kiln can't run beside itself (one writer on its data), and the guard has
    /// nobody to ask, so the checks are probes and queries.
    pub fn of(def: &AppDef) -> Result<Watch, String> {
        let Some(r) = &def.rollout else { return Ok(Watch { window_s: WINDOW.as_secs(), ..Watch::default() }) };
        if r.canary.is_some() || r.linear.is_some() || r.blue_green.is_some() {
            return Err("kiln deploys itself all at once: it can't run beside itself, one writer on its data (rollout: all-at-once: { after, window })".into());
        }
        let plan = kiln_cd::rollout::compile(r, &def.checks)?;
        let after = plan.steps.iter().find_map(|s| if let kiln_cd::rollout::Step::Watch { names, .. } = s { Some(names.clone()) } else { None }).unwrap_or_default();
        let window_ms = match r.all_at_once.as_ref().and_then(|a| a.window.as_deref()) {
            Some(w) => kiln_cd::apps::duration_ms(w).ok_or_else(|| format!("window `{w}`: a duration (30s, 5m, 1h)"))?,
            None if r.all_at_once.is_some() => AFTER_WINDOW.as_millis() as u64,
            None => WINDOW.as_millis() as u64,
        };
        let mut checks = BTreeMap::new();
        for n in &after {
            let Some(c) = def.checks.get(n) else { continue };
            match c.kind()? {
                "approve" => return Err(format!("check {n}: an approval waits for a person, and nobody is there while kiln restarts")),
                _ => super::checks::measurable(c).map_err(|e| format!("check {n}: {e}"))?,
            }
            checks.insert(n.clone(), c.clone());
        }
        Ok(Watch { window_s: window_ms.div_ceil(1000), after, checks, rollout: Value::Null })
    }

    fn window(&self) -> Duration {
        Duration::from_secs(self.window_s)
    }
}

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
    /// What its guard watches once the new pods are up.
    #[serde(default)]
    pub watch: Watch,
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
    let deadline = DEADLINE.as_secs() + m.watch.window_s;
    let mut command = vec![
        json!("/kiln"), json!("cd"), json!("guard"),
        json!("--namespace"), json!(m.namespace), json!("--deployment"), json!(m.deployment), json!("--container"), json!(m.container),
        json!("--previous"), json!(m.from), json!("--expect"), json!(m.to), json!("--deadline"), json!(DEADLINE.as_secs().to_string()),
    ];
    if m.watch.window_s > 0 {
        command.extend([json!("--watch"), json!(serde_json::to_string(&m.watch).unwrap_or_default())]);
    }
    let mut spec = json!({
        "restartPolicy": "Never",
        "containers": [{
            "name": "guard",
            "image": m.from,
            "command": command,
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
    // kiln's container's own security settings, so the guard passes the namespace's pod security
    // (kiln's is `restricted`) as kiln does.
    let kiln = pod.get("containers").and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(m.container.as_str())));
    if let (Some(sc), Some(c)) = (kiln.and_then(|c| c.get("securityContext")), spec.pointer_mut("/containers/0").and_then(Value::as_object_mut)) {
        c.insert("securityContext".into(), sc.clone());
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
    /// What it watches once the new pods are up; a window of 0 doesn't.
    pub watch: Watch,
    /// From outside the cluster (by hand, or a test): a kubeconfig context, and its file.
    pub context: Option<(String, String)>,
}

/// `kiln cd guard`: watches the rollout of `expect` until its pods are up, then for its window;
/// puts `previous` back when anything fails. Runs in a pod, as kiln's service account.
pub async fn run(a: &Args) -> Result<()> {
    let ep = match &a.context {
        Some((context, kubeconfig)) => super::kubeconfig_endpoint(kubeconfig, context)?,
        None => kiln_cd::kube::in_cluster().context("the guard runs in a pod, as kiln's service account (or give it --context)")?,
    };
    let c = Cluster::new("guard", ep)?;
    // Each phase's future on the heap: nested in one another, they'd overflow a shard's stack.
    match Box::pin(until_up(&c, a)).await {
        Start::Up => {}
        Start::Moved => return Ok(()),
        Start::Failed(reason) => return Box::pin(roll_back(&c, a, &reason, Vec::new())).await,
    }
    if a.watch.window_s == 0 {
        println!("{} is available: the deploy of {} took", a.deployment, a.expect);
        let _ = Box::pin(annotate(&c, a, "ready", "", &[], None)).await;
        return Ok(());
    }
    println!("{} is available; watching {} for {} s", a.deployment, a.expect, a.watch.window_s);
    match Box::pin(watch(&c, a)).await {
        Seen::Healthy(checks) => {
            println!("{} stayed healthy for {} s: the deploy took", a.expect, a.watch.window_s);
            let _ = Box::pin(annotate(&c, a, "ready", "", &checks, None)).await;
            Ok(())
        }
        Seen::Moved => {
            println!("{} no longer asks for {}: someone changed it since; nothing to guard", a.deployment, a.expect);
            Ok(())
        }
        Seen::Failed(reason, checks) => Box::pin(roll_back(&c, a, &reason, checks)).await,
    }
}

enum Start {
    Up,
    Moved,
    Failed(String),
}

/// Until the new version's pods are available: they can't progress, or keep restarting, or the
/// deadline passes, and it failed.
async fn until_up(c: &Cluster, a: &Args) -> Start {
    let started = Instant::now();
    loop {
        match Box::pin(read_deployment(c, a)).await {
            Ok(Some(d)) if started.elapsed() < APPLY_GRACE && image_of(obj::get(&d, &["spec", "template"]).unwrap_or(&Value::Null), Some(&a.container)).is_some_and(|(_, i)| i == a.previous) => {}
            Ok(Some(d)) => match judge(&d, &a.container, &a.expect) {
                Verdict::Ready => return Start::Up,
                Verdict::Moved => {
                    println!("{} no longer asks for {}: someone changed it since; nothing to guard", a.deployment, a.expect);
                    return Start::Moved;
                }
                Verdict::Failed(reason) => return Start::Failed(reason),
                Verdict::Waiting => {
                    if let Some(p) = Box::pin(pods(c, a, &d)).await
                        && p.restarts >= CRASH_LOOP
                    {
                        return Start::Failed(format!("its container restarted {} times before it was up{}", p.restarts, p.last_end));
                    }
                }
            },
            Ok(None) => return Start::Failed(format!("{}/{} is gone", a.namespace, a.deployment)),
            Err(e) => eprintln!("reading {}: {e:#}", a.deployment),
        }
        if started.elapsed() >= a.deadline {
            return Start::Failed(format!("its pods weren't available within {} s", a.deadline.as_secs()));
        }
        crate::rt::sleep(LOOK).await;
    }
}

enum Seen {
    /// The window passed: each check's record.
    Healthy(Vec<Value>),
    Moved,
    Failed(String, Vec<Value>),
}

/// Watches the new version for its window: its pods, kiln's server in them, and the app's
/// after checks, measured as they come due.
async fn watch(c: &Cluster, a: &Args) -> Seen {
    let window = Window::new(&a.watch.after, &a.watch.checks, a.watch.window().as_millis() as u64, super::now_ms());
    let until = crate::util::now_unix() + i64::try_from(a.watch.window_s).unwrap_or(i64::MAX);
    let mut w = window;
    let (tx, rx) = async_channel::unbounded::<(u64, Measurement)>();
    let mut unavailable = 0u32;
    let mut looked: Option<Instant> = None;
    let mut said: Option<Instant> = None;
    loop {
        let now = super::now_ms();
        for (check, _, id) in w.due(now) {
            let spec = a.watch.checks.get(&check).cloned().unwrap_or_default();
            let (rollout, tx) = (a.watch.rollout.clone(), tx.clone());
            let timeout = spec.timing(true).ok().and_then(|t| t.timeout_ms).map_or(Duration::from_secs(30), Duration::from_millis);
            crate::rt::spawn(async move {
                let m = super::checks::measure(&spec, &rollout, timeout).await;
                let _ = tx.send((id, m)).await;
            });
        }
        while let Ok((id, m)) = rx.try_recv() {
            w.measured(id, m, super::now_ms());
        }
        if looked.is_none_or(|t| t.elapsed() >= LOOK) {
            looked = Some(Instant::now());
            match Box::pin(read_deployment(c, a)).await {
                Ok(Some(d)) => {
                    match judge(&d, &a.container, &a.expect) {
                        Verdict::Moved => return Seen::Moved,
                        Verdict::Failed(reason) => return Seen::Failed(reason, w.summary()),
                        Verdict::Waiting => unavailable += 1,
                        Verdict::Ready => unavailable = 0,
                    }
                    if unavailable >= UNAVAILABLE_READS {
                        return Seen::Failed(format!("its pods stopped being available ({} reads in a row)", unavailable), w.summary());
                    }
                    if let Some(p) = Box::pin(pods(c, a, &d)).await {
                        if p.restarts > 0 {
                            return Seen::Failed(format!("its container restarted {} time{}{}", p.restarts, if p.restarts == 1 { "" } else { "s" }, p.last_end), w.summary());
                        }
                        if let Some(why) = p.server {
                            return Seen::Failed(why, w.summary());
                        }
                    }
                }
                Ok(None) => return Seen::Failed(format!("{}/{} is gone", a.namespace, a.deployment), w.summary()),
                Err(e) => eprintln!("reading {}: {e:#}", a.deployment),
            }
        }
        match w.verdict(super::now_ms()) {
            Watched::Watching => {}
            Watched::Passed => return Seen::Healthy(w.summary()),
            Watched::Failed(why) => return Seen::Failed(why, w.summary()),
        }
        if said.is_none_or(|t| t.elapsed() >= SAY) {
            said = Some(Instant::now());
            let _ = Box::pin(annotate(c, a, "watching", "", &w.summary(), Some(until))).await;
        }
        let next = Duration::from_millis(w.next_ms().saturating_sub(super::now_ms()));
        crate::rt::sleep(next.clamp(Duration::from_millis(200), Duration::from_secs(1))).await;
    }
}

async fn read_deployment(c: &Cluster, a: &Args) -> Result<Option<Value>> {
    match c.request(Method::GET, &deployment_path(&a.namespace, &a.deployment), None, None).await? {
        (200, bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        (404, _) => Ok(None),
        (status, bytes) => bail!("{}", kiln_cd::kube::status_message(status, &bytes)),
    }
}

/// The new version's pods, as the guard reads them.
#[derive(Debug, Default, PartialEq)]
struct Pods {
    /// Restarts of their containers.
    restarts: i64,
    /// How the last one that restarted ended: ` (last ended: OOMKilled, exit 137)`.
    last_end: String,
    /// kiln's server in one of them was started again, or the OOM killer struck: why.
    server: Option<String>,
}

/// Reads the new version's pods (those of the Deployment running `expect`), and kiln's report of
/// itself on each. None when they can't be read (kiln's service account may not list pods).
async fn pods(c: &Cluster, a: &Args, d: &Value) -> Option<Pods> {
    let selector = obj::get(d, &["spec", "selector", "matchLabels"])?.as_object()?.iter().filter_map(|(k, v)| Some(format!("{k}={}", v.as_str()?))).collect::<Vec<_>>().join(",");
    let path = format!("/api/v1/namespaces/{}/pods?labelSelector={}", kiln_cd::kube::enc(&a.namespace), kiln_cd::kube::enc(&selector));
    let list: Value = match c.request(Method::GET, &path, None, None).await {
        Ok((200, bytes)) => serde_json::from_slice(&bytes).ok()?,
        Ok((status, bytes)) => {
            eprintln!("listing {}'s pods: {}", a.deployment, kiln_cd::kube::status_message(status, &bytes));
            return None;
        }
        Err(e) => {
            eprintln!("listing {}'s pods: {e:#}", a.deployment);
            return None;
        }
    };
    let mut p = new_pods(&list, &a.container, &a.expect);
    for (ip, port) in addresses(&list, &a.container, &a.expect) {
        if p.server.is_none() {
            p.server = Box::pin(server_said(&format!("http://{ip}:{port}{SERVER_HEALTH}"))).await;
        }
    }
    Some(p)
}

/// The restarts of the containers running `expect`, from a pod list.
fn new_pods(list: &Value, container: &str, expect: &str) -> Pods {
    let mut p = Pods::default();
    for pod in list.get("items").and_then(Value::as_array).into_iter().flatten() {
        if image_of(pod, Some(container)).map(|(_, i)| i).as_deref() != Some(expect) {
            continue;
        }
        let statuses = obj::get(pod, &["status", "containerStatuses"]).and_then(Value::as_array);
        for s in statuses.into_iter().flatten().filter(|s| s.get("name").and_then(Value::as_str) == Some(container)) {
            let n = s.get("restartCount").and_then(Value::as_i64).unwrap_or(0);
            if n > 0 {
                p.restarts += n;
                let last = obj::get(s, &["lastState", "terminated"]);
                let reason = last.and_then(|t| t.get("reason")).and_then(Value::as_str).unwrap_or("");
                let code = last.and_then(|t| t.get("exitCode")).and_then(Value::as_i64);
                p.last_end = match (reason, code) {
                    ("", None) => String::new(),
                    ("", Some(c)) => format!(" (last ended: exit {c})"),
                    (r, None) => format!(" (last ended: {r})"),
                    (r, Some(c)) => format!(" (last ended: {r}, exit {c})"),
                };
            }
        }
    }
    p
}

/// Where kiln's server answers on each pod running `expect`: its IP, and its container's `http`
/// port (7070).
fn addresses(list: &Value, container: &str, expect: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for pod in list.get("items").and_then(Value::as_array).into_iter().flatten() {
        let Some(c) = obj::get(pod, &["spec", "containers"]).and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(container))) else { continue };
        if c.get("image").and_then(Value::as_str) != Some(expect) {
            continue;
        }
        let Some(ip) = obj::get(pod, &["status", "podIP"]).and_then(Value::as_str).filter(|i| !i.is_empty()) else { continue };
        let port = c.get("ports").and_then(Value::as_array).and_then(|ps| ps.iter().find(|p| p.get("name").and_then(Value::as_str) == Some("http"))).and_then(|p| p.get("containerPort")).and_then(Value::as_i64).unwrap_or(7070);
        out.push((ip.to_owned(), port));
    }
    out
}

/// What kiln's report of itself says against it: its supervisor started it again, or the OOM
/// killer struck in its container. Nothing when it answers well, or can't be asked (an older
/// server has no report; from outside the cluster its pod can't be reached).
async fn server_said(url: &str) -> Option<String> {
    let client = crate::http::client::Client::public().ok()?;
    let resp = client.request(crate::http::client::Method::Get, url).timeout(SERVER_HEALTH_TIMEOUT).send().await.ok()?;
    if resp.status() != 200 {
        return None;
    }
    let bytes = resp.bytes(64 << 10).await.ok()?;
    server_report(&serde_json::from_slice(&bytes).ok()?)
}

/// Why kiln's report of itself fails a deploy, if it does.
fn server_report(r: &Value) -> Option<String> {
    let restarts = r.get("restarts").and_then(Value::as_u64).unwrap_or(0);
    let oom = obj::get(r, &["memory", "oom_kills"]).and_then(Value::as_u64).unwrap_or(0);
    match (restarts, oom) {
        (0, 0) => None,
        (n, 0) => Some(format!("kiln's server was started again {n} time{} inside its pod", if n == 1 { "" } else { "s" })),
        (0, k) => Some(format!("the kernel's OOM killer struck {k} time{} in its container", if k == 1 { "" } else { "s" })),
        (n, k) => Some(format!("kiln's server was started again {n} time{} inside its pod, the OOM killer having struck {k} time{}", if n == 1 { "" } else { "s" }, if k == 1 { "" } else { "s" })),
    }
}

/// What the guard leaves in its annotation: `watching` (until `until`), `ready` or
/// `rolled-back`, why, and each check's record.
fn note(a: &Args, result: &str, reason: &str, checks: &[Value], until: Option<i64>) -> String {
    let mut n = serde_json::Map::new();
    for (k, v) in [("result", json!(result)), ("from", json!(a.previous)), ("to", json!(a.expect)), ("reason", json!(reason)), ("at", json!(crate::util::now_unix()))] {
        n.insert(k.into(), v);
    }
    if a.watch.window_s > 0 {
        n.insert("window_s".into(), json!(a.watch.window_s));
    }
    if !checks.is_empty() {
        n.insert("checks".into(), json!(checks));
    }
    if let Some(u) = until {
        n.insert("until".into(), json!(u));
    }
    Value::Object(n).to_string()
}

async fn annotate(c: &Cluster, a: &Args, result: &str, reason: &str, checks: &[Value], until: Option<i64>) -> Result<()> {
    patch(c, &a.namespace, &a.deployment, &json!({ "metadata": { "annotations": { ANNOTATION: note(a, result, reason, checks, until) } } })).await
}

async fn roll_back(c: &Cluster, a: &Args, reason: &str, checks: Vec<Value>) -> Result<()> {
    eprintln!("{} didn't take ({reason}): putting {} back", a.expect, a.previous);
    let body = json!({
        "metadata": { "annotations": { ANNOTATION: note(a, "rolled-back", reason, &checks, None) } },
        "spec": { "template": { "spec": { "containers": [{ "name": a.container, "image": a.previous }] } } },
    });
    patch(c, &a.namespace, &a.deployment, &body).await.context("putting the previous image back")?;
    bail!("{} didn't take ({reason}); {} is back", a.expect, a.previous)
}

/// `5m`, `90s`, `1h`.
fn span(secs: u64) -> String {
    match secs {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
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
        // A deploy of kiln still in flight (its guard watching, or its record not finished yet)
        // is over once another starts: its record says how it stood.
        if let Some(h) = have {
            let (succeeded, message) = outcome(&h, &live, Some(&to));
            self.record_self_deploy(&h, succeeded, message).await;
        }
        let app = obj::annotation(object, "kiln.dev/tracking").and_then(|t| t.split_once(':').map(|(a, _)| a.to_owned())).unwrap_or_default();
        let started = self.started.lock().get(&app).cloned();
        let (key, content, commits, artifacts, def, resolved) = {
            let apps = self.apps.read();
            let e = apps.get(&app);
            let r = e.and_then(|e| e.key.and_then(|k| e.resolved.iter().find(|r| r.key == k)));
            (r.map(|r| r.key), r.map(|r| r.content), r.map(|r| r.commits.clone()).unwrap_or_default(), r.map(|r| r.artifacts.clone()).unwrap_or_default(), e.map(|e| e.def.clone()), r.map(|r| r.revision.clone()).filter(|r| !r.is_empty()))
        };
        // The revision deployed: the sync's, else what the app resolved (the commit checks read).
        let revision = started.as_ref().and_then(|s| s.revision.clone()).or(resolved);
        // What its guard watches once the new version is up: its rollout, which can't ask what a
        // deploy of kiln can't do.
        let mut watch = match &def {
            Some(d) => Watch::of(d).map_err(|e| anyhow::anyhow!("{app}'s rollout: {e}"))?,
            None => Watch { window_s: WINDOW.as_secs(), ..Watch::default() },
        };
        let previous = self.store.read(|d| d.history.get(&app).and_then(|h| h.iter().rev().find(|x| x.succeeded).and_then(|x| x.revision.clone())));
        watch.rollout = json!({
            "app": app, "revision": revision, "previous_revision": previous,
            "environment": def.as_ref().and_then(|d| d.environment.clone()), "target": def.as_ref().map(|d| d.destination.cluster.clone()),
            "blue": from, "green": to, "weight": 100, "phase": "after", "against": "production",
        });
        let mut m = Marker {
            app,
            op: started.as_ref().map_or(0, |s| s.op),
            revision,
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
            watch,
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
        tracing::info!(from = %m.from, to = %m.to, guard = ?m.job, window_s = m.watch.window_s, checks = ?m.watch.after, "deployments: deploying kiln itself; a guard watches the restart, then the new version");
        Ok(())
    }

    /// On a restart: finishes the record of a self-deploy that was in flight, once its guard has
    /// said how it went. The Deployment says the rest: the new image means it took; the previous
    /// one means the guard put it back.
    pub(crate) async fn finish_self_deploy(self: &Arc<Self>, cluster: &Cluster) {
        let Some(m) = self.store.read(|d| d.settings.get(SETTING).cloned()).filter(|v| !v.is_null()).and_then(|v| serde_json::from_value::<Marker>(v).ok()) else { return };
        // A guard that watches says when it's done; until then the deploy isn't over. One that
        // never says (it was stopped) is given its whole time, then the Deployment decides.
        let give_up = m.started_ms + (DEADLINE + m.watch.window() + Duration::from_secs(300)).as_millis() as u64;
        let live = loop {
            let read = cluster.request(Method::GET, &deployment_path(&m.namespace, &m.deployment), None, None).await;
            let Ok((200, bytes)) = read else {
                tracing::warn!("deployments: can't read kiln's own Deployment to finish a self-deploy's record; trying at the next start");
                return;
            };
            let Ok(live) = serde_json::from_slice::<Value>(&bytes) else { return };
            let (now, guard) = seen(&m, &live);
            let said = guard.as_ref().and_then(|g| g.get("result")).and_then(Value::as_str).is_some_and(|r| r == "ready" || r == "rolled-back");
            if m.watch.window_s == 0 || said || now != m.to || super::now_ms() >= give_up {
                break live;
            }
            // A newer deploy may have ended this one's watch, and said so.
            if !self.holds(&m) || !self.hub.shutdown.sleep(Duration::from_secs(10)).await {
                return;
            }
        };
        let (succeeded, message) = outcome(&m, &live, None);
        self.record_self_deploy(&m, succeeded, message).await;
    }

    /// Whether the store still holds `m` as the self-deploy in flight.
    fn holds(&self, m: &Marker) -> bool {
        let held: Option<Marker> = self.store.read(|d| d.settings.get(SETTING).cloned()).and_then(|v| serde_json::from_value(v).ok());
        held.is_some_and(|h| h.to == m.to && h.from == m.from && h.started_ms == m.started_ms)
    }

    /// Records a self-deploy's deployment and lets go of its record, unless another took its place.
    async fn record_self_deploy(self: &Arc<Self>, m: &Marker, succeeded: bool, message: String) {
        if !self.holds(m) {
            return;
        }
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

/// The image kiln's Deployment runs now, and its guard's note about `m`'s deploy.
fn seen(m: &Marker, live: &Value) -> (String, Option<Value>) {
    let template = obj::get(live, &["spec", "template"]).cloned().unwrap_or(Value::Null);
    let now = image_of(&template, Some(&m.container)).map(|(_, i)| i).unwrap_or_default();
    let guard = obj::annotation(live, ANNOTATION).and_then(|a| serde_json::from_str::<Value>(a).ok()).filter(|g| g.get("to").and_then(Value::as_str) == Some(m.to.as_str()));
    (now, guard)
}

/// How a self-deploy went, from the Deployment and its guard's note: whether it succeeded, and
/// the record's message. `ended`: a newer deploy, of that image, ended its watch.
fn outcome(m: &Marker, live: &Value, ended: Option<&str>) -> (bool, String) {
    let (now, guard) = seen(m, live);
    let watched = guard.as_ref().and_then(|g| g.get("checks")).and_then(Value::as_array).map(|cs| cs.iter().filter_map(|c| c.get("check").and_then(Value::as_str)).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty());
    let ready = guard.as_ref().and_then(|g| g.get("result")).and_then(Value::as_str) == Some("ready");
    if now == m.to {
        let how = match (m.watch.window_s, ready, watched, ended) {
            (_, false, _, Some(newer)) => format!("; deploying {newer} ended its watch"),
            (0, _, _, _) => String::new(),
            (w, true, Some(c), _) => format!(", and stayed healthy for {} (checks: {c})", span(w)),
            (w, true, None, _) => format!(", and stayed healthy for {}", span(w)),
            (_, false, _, None) => "; its guard didn't say how its watch ended".into(),
        };
        (true, format!("kiln restarted into {}{how}", m.to))
    } else if now == m.from {
        let why = guard.as_ref().and_then(|g| g.get("reason").and_then(Value::as_str)).filter(|r| !r.is_empty()).unwrap_or("it didn't start");
        (false, format!("{} didn't take ({why}); the guard put {} back", m.to, m.from))
    } else {
        (false, format!("kiln's Deployment runs {now}, neither the image deployed nor the one before: someone changed it since"))
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
        let template = json!({ "spec": {
            "serviceAccountName": "kiln",
            "imagePullSecrets": [{ "name": "kiln-registry" }],
            "securityContext": { "runAsNonRoot": true },
            "containers": [{ "name": "kiln", "securityContext": { "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } } }],
        } });
        let j = job(&m, &template);
        assert_eq!(j.pointer("/spec/template/spec/containers/0/image").and_then(Value::as_str), Some("acme/kiln@sha256:old"));
        assert_eq!(j.pointer("/spec/template/spec/serviceAccountName").and_then(Value::as_str), Some("kiln"));
        assert_eq!(j.pointer("/spec/template/spec/imagePullSecrets/0/name").and_then(Value::as_str), Some("kiln-registry"));
        let cmd: Vec<&str> = j.pointer("/spec/template/spec/containers/0/command").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        assert_eq!(cmd.get(..3), Some(&["/kiln", "cd", "guard"][..]));
        assert!(cmd.windows(2).any(|w| w == ["--expect", "acme/kiln@sha256:new"]));
        assert_eq!(j.pointer("/spec/backoffLimit").and_then(Value::as_i64), Some(0));
        // It meets the namespace's pod security as kiln's own pod does.
        assert_eq!(j.pointer("/spec/template/spec/securityContext/runAsNonRoot").and_then(Value::as_bool), Some(true));
        assert_eq!(j.pointer("/spec/template/spec/containers/0/securityContext/allowPrivilegeEscalation").and_then(Value::as_bool), Some(false));
    }

    /// kiln's app from a deploy file with this rollout (none: no `rollout:`) and these checks.
    fn app(rollout: Option<&str>, checks: &str) -> Result<AppDef, String> {
        let checks: String = checks.lines().map(|l| format!("  {l}\n")).collect();
        let rollout = rollout.map(|r| format!("    rollout: {{ {r} }}\n")).unwrap_or_default();
        let text = format!("apps:\n  kiln:\n    source: deploy/helm/kiln\n{rollout}checks:\n{checks}");
        let apps = kiln_cd::apps::from_deploy_file(&text, "hyper-light/kiln", "main", &BTreeMap::new(), &kiln_cd::apps::Scope::default()).map_err(|e| e.0)?;
        apps.into_iter().next().ok_or_else(|| "no app".to_owned())
    }

    /// What a deploy of kiln watches comes from its app's rollout, which can ask only what such
    /// a deploy can do.
    #[test]
    fn what_the_guard_watches() -> Result<(), String> {
        let checks = "steady: { http: { url: http://kiln:7070/healthz/server }, every: 10s }\nerrors: { metric: { prometheus: { query: up, address: http://p:9090 } }, every: 1m, fail: \"${{ result > 1 }}\" }\nok: { approve: {} }\ne2e: { script: { run: ./e2e.sh } }";
        let w = Watch::of(&app(None, checks)?)?;
        assert_eq!((w.window_s, w.after.len()), (300, 0), "no rollout: five minutes of its pods and kiln's server, no checks of its own");
        let w = Watch::of(&app(Some("all-at-once: { after: [steady, errors], window: 10m }"), checks)?)?;
        assert_eq!((w.window_s, w.after.clone()), (600, vec!["steady".to_owned(), "errors".to_owned()]));
        assert_eq!(w.checks.keys().collect::<Vec<_>>(), ["errors", "steady"], "only the checks it measures go to the guard");
        assert_eq!(Watch::of(&app(Some("all-at-once: {}"), checks)?)?.window_s, 600, "an all-at-once rollout's after window");
        assert_eq!(Watch::of(&app(Some("all-at-once: { window: 2m }"), checks)?)?.window_s, 120, "a window of its own, with no checks");
        assert!(Watch::of(&app(Some("blue-green: { after: steady }"), checks)?).is_err_and(|e| e.contains("all at once")));
        assert!(Watch::of(&app(Some("canary: [{ weight: 10 }]"), checks)?).is_err_and(|e| e.contains("all at once")));
        assert!(Watch::of(&app(Some("all-at-once: { after: ok }"), checks)?).is_err_and(|e| e.contains("nobody is there")));
        assert!(Watch::of(&app(Some("all-at-once: { after: e2e }"), checks)?).is_err_and(|e| e.contains("not built yet")));
        assert!(app(Some("all-at-once: { after: nothing }"), checks).is_err_and(|e| e.contains("no check named nothing")), "the deploy file doesn't read");
        Ok(())
    }

    #[test]
    fn the_guard_is_told_what_to_watch() {
        let watch = Watch { window_s: 600, after: vec!["steady".into()], checks: serde_yaml::from_str("steady: { http: { url: x } }").unwrap_or_default(), rollout: json!({ "revision": "abc" }) };
        let m = Marker { namespace: "kiln".into(), deployment: "kiln".into(), container: "kiln".into(), from: "kiln:old".into(), to: "kiln:new".into(), watch: watch.clone(), ..Marker::default() };
        let j = job(&m, &json!({ "spec": {} }));
        let cmd: Vec<&str> = j.pointer("/spec/template/spec/containers/0/command").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let given = cmd.windows(2).find_map(|w| match w {
            ["--watch", v] => Some(*v),
            _ => None,
        });
        let given = given.unwrap_or("");
        assert_eq!(serde_json::from_str::<Watch>(given).ok(), Some(watch), "{cmd:?}");
        assert!(cmd.windows(2).any(|w| w == ["--deadline", "600"]), "the deadline to be up stays its own");
        assert_eq!(j.pointer("/spec/activeDeadlineSeconds").and_then(Value::as_u64), Some(600 + 600 + 300), "the Job lives through the window");
        // A record kept before windows reads as one with none, and its guard isn't told to watch.
        let old: Marker = serde_json::from_value(json!({ "app": "kiln", "op": 1, "revision": null, "key": null, "content": null, "commits": [], "artifacts": [], "trigger": "manual", "by": null, "started_ms": 1, "namespace": "kiln", "deployment": "kiln", "container": "kiln", "from": "a", "to": "b", "job": null })).unwrap_or_default();
        assert_eq!((old.to.as_str(), old.watch.window_s), ("b", 0));
        let j = job(&old, &json!({ "spec": {} }));
        assert!(!j.to_string().contains("--watch"));
    }

    #[test]
    fn the_guard_reads_the_new_versions_pods() {
        let pod = |image: &str, ip: &str, restarts: i64, last: Value| json!({
            "spec": { "containers": [{ "name": "kiln", "image": image, "ports": [{ "name": "http", "containerPort": 7070 }] }] },
            "status": { "podIP": ip, "containerStatuses": [{ "name": "kiln", "restartCount": restarts, "lastState": last }] },
        });
        let list = json!({ "items": [
            pod("kiln:old", "10.0.0.1", 9, json!({})),
            pod("kiln:new", "10.0.0.2", 2, json!({ "terminated": { "reason": "OOMKilled", "exitCode": 137 } })),
        ] });
        let p = new_pods(&list, "kiln", "kiln:new");
        assert_eq!((p.restarts, p.last_end.as_str()), (2, " (last ended: OOMKilled, exit 137)"), "only the new version's pods count");
        assert_eq!(new_pods(&list, "kiln", "kiln:other"), Pods::default());
        assert_eq!(addresses(&list, "kiln", "kiln:new"), [("10.0.0.2".to_owned(), 7070)]);
        let starting = json!({ "items": [{ "spec": { "containers": [{ "name": "kiln", "image": "kiln:new" }] }, "status": {} }] });
        assert!(addresses(&starting, "kiln", "kiln:new").is_empty(), "a pod with no IP yet isn't asked");
    }

    /// kiln's report of itself fails a deploy when its supervisor had to start it again, or the
    /// OOM killer struck: what the pod's status didn't show on 2026-10-18.
    #[test]
    fn kilns_report_of_itself() {
        assert_eq!(server_report(&json!({ "restarts": 0, "memory": { "oom_kills": 0 } })), None);
        assert_eq!(server_report(&json!({ "commit": "abc" })), None, "an older report says neither");
        assert_eq!(server_report(&json!({ "restarts": 3, "memory": { "oom_kills": 3 } })).as_deref(), Some("kiln's server was started again 3 times inside its pod, the OOM killer having struck 3 times"));
        assert_eq!(server_report(&json!({ "restarts": 1, "memory": null })).as_deref(), Some("kiln's server was started again 1 time inside its pod"));
        assert_eq!(server_report(&json!({ "restarts": 0, "memory": { "oom_kills": 1 } })).as_deref(), Some("the kernel's OOM killer struck 1 time in its container"));
        assert_eq!(span(600), "10m");
        assert_eq!(span(90), "90s");
        assert_eq!(span(7200), "2h");
    }

    /// kiln's own deploy file asks what its guard can do, and its checks mean what they say: a
    /// healthy kiln passes each, and each fails the kiln it is there to catch.
    #[test]
    fn kilns_own_checks() -> Result<(), String> {
        use kiln_cd::rollout::Verdict as V;
        let read = |f: &str| std::fs::read_to_string(format!("{}/../../.kiln/{f}", env!("CARGO_MANIFEST_DIR"))).map_err(|e| format!("{f}: {e}"));
        let environments: BTreeMap<String, kiln_cd::apps::EnvironmentSpec> = serde_yaml::from_str(&read("environments.yaml")?).map_err(|e| e.to_string())?;
        let apps = kiln_cd::apps::from_deploy_file(&read("deploy.yaml")?, "hyper-light/kiln", "main", &environments, &kiln_cd::apps::Scope::default()).map_err(|e| e.0)?;
        let kiln = apps.iter().find(|a| a.name == "kiln-production").ok_or_else(|| format!("no kiln-production in {:?}", apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>()))?;
        let w = Watch::of(kiln)?;
        assert_eq!(w.window_s, 600);
        assert_eq!(w.after, ["serving", "steady", "memory", "commit", "runners"]);
        let rollout = json!({ "revision": "88a939a4405f4aaae76c2964304dfd7c4a69b4a7" });
        let healthy = json!({ "status": 200, "json": {
            "commit": "88a939a4405f4aaae76c2964304dfd7c4a69b4a7", "restarts": 0,
            "memory": { "used": 450_000_000u64, "limit": 2_147_483_648u64, "share": 0.21, "oom_kills": 0 },
            "runners": { "connected": 6, "before": 7, "back": 0.857 },
        } });
        let verdict = |check: &str, result: &Value| {
            let Some(spec) = w.checks.get(check) else { return V::Error };
            let status = result.get("status").and_then(Value::as_u64).unwrap_or(0);
            let default = if (200..300).contains(&status) { (V::Pass, String::new()) } else { (V::Fail, format!("HTTP {status}")) };
            super::super::checks::judge(spec, result, &rollout, default).0
        };
        for c in &w.after {
            assert_eq!(verdict(c, &healthy), V::Pass, "{c}");
        }
        let with = |pointer: &str, v: Value| {
            let mut r = healthy.clone();
            if let Some(x) = r.pointer_mut(pointer) {
                *x = v;
            }
            r
        };
        assert_eq!(verdict("serving", &with("/status", json!(503))), V::Fail, "its store doesn't answer");
        assert_eq!(verdict("steady", &with("/json/restarts", json!(2))), V::Fail, "its supervisor started it again");
        assert_eq!(verdict("steady", &with("/json/memory/oom_kills", json!(1))), V::Fail, "the OOM killer struck");
        assert_eq!(verdict("memory", &with("/json/memory/share", json!(0.93))), V::Fail, "near its limit");
        assert_eq!(verdict("commit", &with("/json/commit", json!(""))), V::Fail, "built knowing no commit");
        assert_eq!(verdict("commit", &with("/json/commit", json!("2bd78af9737dfedded412ad22fafe56de7adfdbe"))), V::Fail, "another commit");
        assert_eq!(verdict("runners", &with("/json/runners/back", json!(0.25))), V::Fail, "most runners didn't come back");
        // Where the kernel says nothing of memory, the memory checks don't decide against it.
        let unlimited = with("/json/memory", Value::Null);
        assert_eq!((verdict("steady", &unlimited), verdict("memory", &unlimited)), (V::Pass, V::Pass));
        Ok(())
    }

    /// How a self-deploy's record reads, from the Deployment and its guard's note.
    #[test]
    fn how_a_self_deploy_went() {
        let m = Marker { container: "kiln".into(), from: "kiln:old".into(), to: "kiln:new".into(), watch: Watch { window_s: 600, ..Watch::default() }, ..Marker::default() };
        let live = |image: &str, note: Value| json!({
            "metadata": { "annotations": { ANNOTATION: note.to_string() } },
            "spec": { "template": { "spec": { "containers": [{ "name": "kiln", "image": image }] } } },
        });
        let checks = json!([{ "check": "serving", "verdict": "pass" }, { "check": "steady", "verdict": "pass" }]);
        let ready = live("kiln:new", json!({ "result": "ready", "to": "kiln:new", "checks": checks }));
        assert_eq!(outcome(&m, &ready, None), (true, "kiln restarted into kiln:new, and stayed healthy for 10m (checks: serving, steady)".into()));
        let back = live("kiln:old", json!({ "result": "rolled-back", "to": "kiln:new", "reason": "steady: failed: the server restarted" }));
        assert_eq!(outcome(&m, &back, None), (false, "kiln:new didn't take (steady: failed: the server restarted); the guard put kiln:old back".into()));
        let watching = live("kiln:new", json!({ "result": "watching", "to": "kiln:new" }));
        assert_eq!(outcome(&m, &watching, Some("kiln:newer")), (true, "kiln restarted into kiln:new; deploying kiln:newer ended its watch".into()));
        assert_eq!(outcome(&m, &watching, None).1, "kiln restarted into kiln:new; its guard didn't say how its watch ended");
        // A note about another deploy says nothing of this one.
        let other = live("kiln:old", json!({ "result": "rolled-back", "to": "kiln:else", "reason": "no" }));
        assert_eq!(outcome(&m, &other, None).1, "kiln:new didn't take (it didn't start); the guard put kiln:old back");
        assert!(outcome(&m, &live("kiln:else", json!({})), None).1.contains("someone changed it since"));
        let before_windows = Marker { watch: Watch::default(), ..m };
        assert_eq!(outcome(&before_windows, &live("kiln:new", json!({})), None), (true, "kiln restarted into kiln:new".into()));
    }
}
