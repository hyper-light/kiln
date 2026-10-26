//! Argo CD as a CD provider (docs/deploy-api.md §8.3), through its REST API with an account's
//! token. An Argo CD instance is an integration of kind `argocd` (Settings → Integrations): its
//! URL, the namespace its Applications live in, a CA to trust when its certificate isn't public,
//! the address it reaches kiln at when that isn't kiln's public URL (`kiln_url`), and the token,
//! sealed in kiln's secret store.
//!
//! An environment on Argo CD has one ApplicationSet, `kiln-<env>`, whose git files generator
//! reads every `release.json` of the environment in its release repository and makes one
//! Application per registration, `<env>-<app>`: the package at its version, its values from the
//! release repository (a second source, `$release`), what it sets and `kiln.release=<id>` (txst's
//! deployment id) as Helm parameters, in the environment's AppProject, `kiln-<env>`.
//! kiln registers the release repository with Argo CD with a token of its own that reads only
//! that repository. A release is a commit and a refresh; it has landed when Argo CD compared its
//! spec (the parameter, or the version for plain manifests) with the release repository at a
//! commit that holds its files, and reports it synced and healthy. Where ApplicationSets aren't
//! allowed, kiln writes each Application itself (`argocd: { applications: true }`), the same
//! spec with the values inline.
//!
//! kiln follows the Applications it made through Argo CD's stream of them ([`Watches`]), and
//! asks again only while the stream is down.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::model::{App, Environment, Package, Registration, ReleaseFile};
use super::provider::{Landed, Undelivered};
use super::repo;
use crate::http::client::{Client, Method, Roots};
use crate::hub::Hub;

/// The integration kind of an Argo CD instance.
pub const KIND: &str = "argocd";

/// The Helm parameter a release's Application carries, read back from what Argo CD compared.
pub const RELEASE_PARAM: &str = "kiln.release";
/// The secrets' versions a release pins, as Helm reads them (`API_KEY@1 DB_PASSWORD@4`): kiln-base
/// stamps them on the pod template, so pods roll onto new ones (docs/deploy-api.md §3.5).
pub const SECRETS_PARAM: &str = "kiln.secrets";

/// An Argo CD instance, reached with its integration's token.
pub struct Argo {
    client: Client,
    base: String,
    token: String,
    /// Where its Applications live.
    pub namespace: String,
    /// The integration it is, by name.
    pub integration: String,
    /// The address it reaches kiln at (release repositories are read there), when that isn't
    /// kiln's public URL.
    pub kiln_url: Option<String>,
}

impl Argo {
    /// The instance an environment names (`argocd/<integration>`).
    pub async fn of(hub: &Hub, integration: &str) -> Result<Self, String> {
        let i = crate::ci::integrations::get(hub, integration).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no integration {integration}"))?;
        if i.kind != KIND {
            return Err(format!("{integration} is a {} integration, not Argo CD", i.kind));
        }
        let field = |k: &str| i.config.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let base = field("url").ok_or_else(|| format!("{integration} has no url"))?.trim_end_matches('/').to_owned();
        let token = crate::ci::integrations::token(hub, &i).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("{integration} has no token stored"))?;
        let mut argo = Self::new(&base, &token, field("namespace").as_deref(), field("ca").as_deref())?;
        argo.integration = integration.to_owned();
        argo.kiln_url = field("kiln_url").map(|u| u.trim_end_matches('/').to_owned());
        Ok(argo)
    }

    pub fn new(base: &str, token: &str, namespace: Option<&str>, ca_pem: Option<&str>) -> Result<Self, String> {
        let client = match ca_pem {
            Some(pem) => Client::new(Roots::Pem(pem.as_bytes().to_vec()), None),
            None => Client::public(),
        }
        .map_err(|e| format!("{e:#}"))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            namespace: namespace.unwrap_or("argocd").to_owned(),
            integration: String::new(),
            kiln_url: None,
        })
    }

    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
        let mut req = self.client.request(method, &format!("{}{path}", self.base)).bearer(&self.token).timeout(std::time::Duration::from_secs(30));
        req = match body {
            Some(b) => req.json(b),
            // Argo CD answers 415 to a change without a JSON content type, even a DELETE.
            None if !matches!(method, Method::Get | Method::Head) => req.header("content-type", "application/json"),
            None => req,
        };
        let resp = req.send().await.map_err(|e| format!("Argo CD at {}: {e:#}", self.base))?;
        let status = resp.status();
        let bytes = resp.bytes(32 << 20).await.map_err(|e| format!("Argo CD: {e:#}"))?;
        let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        Ok((status, v))
    }

    async fn ok(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let (status, v) = self.call(method, path, body).await?;
        if (200..300).contains(&status) {
            return Ok(v);
        }
        Err(answered(status, &v))
    }

    /// Its version, and that the token works.
    pub async fn version(&self) -> Result<String, String> {
        let v = self.ok(Method::Get, "/api/version", None).await?;
        Ok(v.get("Version").and_then(Value::as_str).unwrap_or("?").to_owned())
    }

    /// Whether the token may `action` on `resource` (`applications`, `create`, `default/*`).
    pub async fn can_i(&self, resource: &str, action: &str, sub: &str) -> Result<bool, String> {
        let v = self.ok(Method::Get, &format!("/api/v1/account/can-i/{resource}/{action}/{}", enc(sub)), None).await?;
        Ok(v.get("value").and_then(Value::as_str) == Some("yes"))
    }

    fn app_query(&self) -> String {
        format!("appNamespace={}", enc(&self.namespace))
    }

    /// An Application, refreshed first when `refresh` says (`normal` or `hard`); `None` when there
    /// is none.
    pub async fn application(&self, name: &str, refresh: Option<&str>) -> Result<Option<Value>, String> {
        let refresh = refresh.map(|r| format!("&refresh={r}")).unwrap_or_default();
        let (status, v) = self.call(Method::Get, &format!("/api/v1/applications/{}?{}{refresh}", enc(name), self.app_query()), None).await?;
        match status {
            200 => Ok(Some(v)),
            404 => Ok(None),
            // Argo CD answers 403 for an Application that doesn't exist, as for one the token can't
            // see: its list, which leaves out what the token can't see, tells them apart.
            403 if v.to_string().contains("not found") => Ok(None),
            403 => {
                let (listed, list) = self.call(Method::Get, &format!("/api/v1/applications?name={}&{}", enc(name), self.app_query()), None).await?;
                let there = list.get("items").and_then(Value::as_array).is_some_and(|items| !items.is_empty());
                if (200..300).contains(&listed) && !there { Ok(None) } else { Err(format!("Argo CD answered 403 for Application {name}: {v}")) }
            }
            s => Err(format!("Argo CD answered {s} for Application {name}: {v}")),
        }
    }

    /// Creates or replaces an Application. Argo CD renders it first: one it won't take is refused.
    pub async fn put_application(&self, app: &Value) -> Result<(), Undelivered> {
        let (status, v) = self.call(Method::Post, "/api/v1/applications?upsert=true&validate=true", Some(app)).await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(Undelivered::of_status(status, answered(status, &v)))
    }

    /// Deletes an Application: what it deployed too, unless `keep`.
    pub async fn delete_application(&self, name: &str, keep: bool) -> Result<(), String> {
        let (status, v) = self.call(Method::Delete, &format!("/api/v1/applications/{}?cascade={}&{}", enc(name), !keep, self.app_query()), None).await?;
        match status {
            200..=299 | 404 => Ok(()),
            s => Err(format!("Argo CD answered {s} deleting {name}: {v}")),
        }
    }

    /// Takes off an Application the finalizer that deletes what it runs with it: deleted after
    /// this, whoever deletes it, what it runs stays.
    pub async fn keep_objects(&self, name: &str) -> Result<(), String> {
        let Some(app) = self.application(name, None).await? else { return Ok(()) };
        let finalizers: Vec<Value> = app.pointer("/metadata/finalizers").and_then(Value::as_array).cloned().unwrap_or_default();
        let kept: Vec<Value> = finalizers.iter().filter(|f| !f.as_str().is_some_and(|f| f.starts_with("resources-finalizer.argocd.argoproj.io"))).cloned().collect();
        if kept.len() == finalizers.len() {
            return Ok(());
        }
        let patch = json!({ "metadata": { "finalizers": kept } }).to_string();
        let body = json!({ "name": name, "appNamespace": self.namespace, "patch": patch, "patchType": "merge" });
        self.ok(Method::Patch, &format!("/api/v1/applications/{}", enc(name)), Some(&body)).await.map(|_| ())
    }

    /// Syncs an Application now (an environment that syncs on demand, or a paused one).
    pub async fn sync(&self, name: &str, prune: bool) -> Result<(), String> {
        let body = json!({ "name": name, "appNamespace": self.namespace, "prune": prune });
        self.ok(Method::Post, &format!("/api/v1/applications/{}/sync", enc(name)), Some(&body)).await.map(|_| ())
    }

    /// Turns an Application's automated sync off (`None`) or on (prune, self-heal), git untouched.
    pub async fn set_automation(&self, name: &str, automated: Option<(bool, bool)>) -> Result<(), String> {
        let Some(mut app) = self.application(name, None).await? else { return Ok(()) };
        let Some(spec) = app.get_mut("spec").and_then(Value::as_object_mut) else { return Err(format!("Application {name} has no spec")) };
        let policy = spec.entry("syncPolicy").or_insert_with(|| json!({}));
        if let Some(p) = policy.as_object_mut() {
            match automated {
                Some((prune, self_heal)) => p.insert("automated".into(), json!({ "prune": prune, "selfHeal": self_heal })),
                None => p.remove("automated"),
            };
        }
        let spec = Value::Object(spec.clone());
        self.ok(Method::Put, &format!("/api/v1/applications/{}/spec?{}", enc(name), self.app_query()), Some(&spec)).await.map(|_| ())
    }

    /// The manifests an Application renders: at `revision` (a chart's version) when given, for its
    /// source at `position` (from 1) when it has several.
    pub async fn manifests(&self, name: &str, revision: Option<&str>, position: Option<usize>) -> Result<Vec<Value>, String> {
        let rev = match (revision, position) {
            (Some(r), Some(p)) => format!("&revisions={}&sourcePositions={p}", enc(r)),
            (Some(r), None) => format!("&revision={}", enc(r)),
            (None, _) => String::new(),
        };
        let v = self.ok(Method::Get, &format!("/api/v1/applications/{}/manifests?{}{rev}", enc(name), self.app_query()), None).await?;
        let objects = v.get("manifests").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|s| serde_json::from_str::<Value>(s).ok());
        // Helm's test hooks are rendered, and never synced: Argo CD runs no `helm test`.
        Ok(objects.filter(|o| !o.pointer("/metadata/annotations/helm.sh~1hook").and_then(Value::as_str).is_some_and(|h| h.contains("test"))).collect())
    }

    /// What Argo CD compares of an Application: each object's live, normalized and predicted states.
    pub async fn managed_resources(&self, name: &str) -> Result<Vec<Value>, String> {
        let v = self.ok(Method::Get, &format!("/api/v1/applications/{}/managed-resources?{}", enc(name), self.app_query()), None).await?;
        Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// The last `tail` lines of an object's pods' logs: `{pod, ts, text}`, oldest first, as KilnCD's.
    #[allow(clippy::too_many_arguments)]
    pub async fn logs(&self, name: &str, group: &str, kind: &str, object: &str, namespace: Option<&str>, container: Option<&str>, tail: u32) -> Result<Vec<Value>, String> {
        let mut q = format!("{}&group={}&kind={}&resourceName={}&tailLines={tail}&follow=false", self.app_query(), enc(group), enc(kind), enc(object));
        if let Some(ns) = namespace {
            q.push_str(&format!("&namespace={}", enc(ns)));
        }
        if let Some(c) = container {
            q.push_str(&format!("&container={}", enc(c)));
        }
        let (status, v) = self.call(Method::Get, &format!("/api/v1/applications/{}/logs?{q}", enc(name)), None).await?;
        if !(200..300).contains(&status) {
            return Err(answered(status, &v));
        }
        // A stream of `{"result": {content, timeStamp, podName}}`, one a line.
        let text = match &v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|e| e.get("result").cloned())
            .filter(|r| r.get("last").and_then(Value::as_bool) != Some(true))
            .map(|r| json!({ "pod": r.get("podName"), "ts": r.get("timeStamp"), "text": r.get("content") }))
            .collect())
    }

    /// How it tracks the objects its Applications deploy, as KilnCD reads that tracking to take
    /// them over: by annotation (Argo CD 3's default), label, or both.
    pub async fn tracking(&self) -> Result<kiln_cd::model::Tracking, String> {
        use kiln_cd::model::Tracking;
        let v = self.ok(Method::Get, "/api/v1/settings", None).await?;
        Ok(match v.get("trackingMethod").and_then(Value::as_str).unwrap_or("annotation") {
            "label" => Tracking::ArgoLabel,
            "annotation+label" => Tracking::ArgoAnnotationAndLabel,
            _ => Tracking::ArgoAnnotation,
        })
    }

    fn set_query(&self) -> String {
        format!("appsetNamespace={}", enc(&self.namespace))
    }

    /// An ApplicationSet; `None` when there is none.
    pub async fn application_set(&self, name: &str) -> Result<Option<Value>, String> {
        let (status, v) = self.call(Method::Get, &format!("/api/v1/applicationsets/{}?{}", enc(name), self.set_query()), None).await?;
        match status {
            200 => Ok(Some(v)),
            404 => Ok(None),
            403 if v.to_string().contains("not found") => Ok(None),
            s => Err(format!("{} (ApplicationSet {name})", answered(s, &v))),
        }
    }

    /// Creates or replaces an ApplicationSet.
    pub async fn put_application_set(&self, set: &Value) -> Result<(), Undelivered> {
        let (status, v) = self.call(Method::Post, "/api/v1/applicationsets?upsert=true", Some(set)).await?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(Undelivered::of_status(status, answered(status, &v)))
    }

    /// Deletes an ApplicationSet. Its Applications stay: kiln's never delete theirs.
    pub async fn delete_application_set(&self, name: &str) -> Result<(), String> {
        let (status, v) = self.call(Method::Delete, &format!("/api/v1/applicationsets/{}?{}", enc(name), self.set_query()), None).await?;
        match status {
            200..=299 | 404 => Ok(()),
            s => Err(format!("{} deleting ApplicationSet {name}", answered(s, &v))),
        }
    }

    /// Creates or replaces an AppProject.
    pub async fn put_project(&self, project: &Value) -> Result<(), String> {
        self.ok(Method::Post, "/api/v1/projects", Some(&json!({ "project": project, "upsert": true }))).await.map(|_| ())
    }

    /// A repository Argo CD reads, and how connecting to it goes now (`forceRefresh`: Argo CD tries
    /// it again; its cached answer can say a credential that was revoked still reads); `None` when
    /// it has none. Argo CD answers 403 for a repository it doesn't have, as for one the token may
    /// not see: putting it says which.
    pub async fn repository(&self, url: &str) -> Result<Option<Value>, String> {
        let (status, v) = self.call(Method::Get, &format!("/api/v1/repositories/{}?forceRefresh=true", enc(url)), None).await?;
        match status {
            200 => Ok(Some(v)),
            403 | 404 => Ok(None),
            s => Err(format!("{} (repository {url})", answered(s, &v))),
        }
    }

    /// Makes Argo CD read a git repository as `username` with `password` (a token of kiln's). It
    /// tries the repository first: one it can't read is refused, with why.
    pub async fn put_repository(&self, url: &str, name: &str, username: &str, password: &str) -> Result<(), String> {
        let body = json!({ "repo": url, "type": "git", "name": name, "username": username, "password": password });
        self.ok(Method::Post, "/api/v1/repositories?upsert=true", Some(&body)).await.map(|_| ())
    }

    /// Makes Argo CD read a git repository over SSH with `private_key` (a deploy key of kiln's),
    /// tried first as [`Self::put_repository`] is. Argo CD checks the server's host key against
    /// the ones it knows (`argocd-ssh-known-hosts-cm`).
    pub async fn put_repository_ssh(&self, url: &str, name: &str, private_key: &str) -> Result<(), String> {
        let body = json!({ "repo": url, "type": "git", "name": name, "sshPrivateKey": private_key });
        self.ok(Method::Post, "/api/v1/repositories?upsert=true", Some(&body)).await.map(|_| ())
    }

    fn object_query(&self, o: &Object) -> String {
        format!("{}&namespace={}&resourceName={}&version={}&group={}&kind={}", self.app_query(), enc(&o.namespace), enc(&o.name), enc(&o.version), enc(&o.group), enc(&o.kind))
    }

    /// The actions Argo CD offers on an object of an Application (restart, pause, suspend …), each
    /// `{name, disabled, params?}`.
    pub async fn actions(&self, app: &str, o: &Object) -> Result<Vec<Value>, String> {
        let v = self.ok(Method::Get, &format!("/api/v1/applications/{}/resource/actions?{}", enc(app), self.object_query(o)), None).await?;
        Ok(v.get("actions").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Runs one of an object's actions, with its parameters (a scale's replicas).
    pub async fn run_action(&self, app: &str, o: &Object, action: &str, params: &BTreeMap<String, String>) -> Result<(), String> {
        let params: Vec<Value> = params.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect();
        let body = json!({
            "name": app, "appNamespace": self.namespace, "namespace": o.namespace, "resourceName": o.name,
            "version": o.version, "group": o.group, "kind": o.kind, "action": action, "resourceActionParameters": params,
        });
        self.ok(Method::Post, &format!("/api/v1/applications/{}/resource/actions/v2", enc(app)), Some(&body)).await.map(|_| ())
    }

    /// Deletes one object of an Application (a pod, for its workload to make again).
    pub async fn delete_object(&self, app: &str, o: &Object) -> Result<(), String> {
        self.ok(Method::Delete, &format!("/api/v1/applications/{}/resource?{}", enc(app), self.object_query(o)), None).await.map(|_| ())
    }

    /// Argo CD's stream of the Applications `selector` picks: each change as it happens, a JSON
    /// object a line (`{"result": {"type", "application"}}`).
    pub async fn stream(&self, selector: &str) -> Result<crate::http::client::Response, String> {
        let url = format!("{}/api/v1/stream/applications?selector={}&{}", self.base, enc(selector), self.app_query());
        let resp = self.client.request(Method::Get, &url).bearer(&self.token).timeout(Duration::from_secs(30)).send().await.map_err(|e| format!("Argo CD at {}: {e:#}", self.base))?;
        if !resp.is_success() {
            let status = resp.status();
            let bytes = resp.bytes(1 << 20).await.unwrap_or_default();
            let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            return Err(answered(status, &v));
        }
        Ok(resp)
    }

    /// Makes Argo CD know an OCI Helm registry (public: no credentials).
    pub async fn ensure_oci_repo(&self, url: &str) -> Result<(), String> {
        let body = json!({ "repo": url, "type": "helm", "enableOCI": true, "name": url.replace(['/', ':', '.'], "-") });
        self.ok(Method::Post, "/api/v1/repositories?upsert=true", Some(&body)).await.map(|_| ())
    }
}

fn enc(s: &str) -> String {
    kiln_cd::kube::enc(s)
}

/// What Argo CD said, when it said no.
fn answered(status: u16, v: &Value) -> String {
    let why = v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).map_or_else(|| v.to_string(), str::to_owned);
    format!("Argo CD answered {status}: {why}")
}

/// The Application of a registration.
pub fn application_name(app: &App, env: &str) -> String {
    format!("{env}-{}", app.name)
}

/// An OCI chart repository as Argo CD names it: without the scheme.
fn argo_repo(repo: &str) -> (String, bool) {
    match repo.strip_prefix("oci://") {
        Some(rest) => (rest.trim_end_matches('/').to_owned(), true),
        None => (repo.to_owned(), false),
    }
}

/// The Application a release makes, where kiln writes them itself: the package at its version,
/// the environment's values inline, what it sets and `kiln.release` stamped where Helm renders.
pub fn application(namespace: &str, app: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, values: Option<&str>, commit: &str) -> Result<Value, String> {
    let version = file.version.clone().ok_or("the release names no version")?;
    let mut set = repo::set_of(file)?;
    let values: Value = match values.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => serde_yaml::from_str::<Value>(v).map_err(|e| format!("values.yaml doesn't read: {e}"))?,
        None => Value::Null,
    };
    let values = if values.as_object().is_some_and(|m| !m.is_empty()) { Some(values) } else { None };
    let helm_wanted = matches!(file.package, Package::Helm { .. }) || !set.is_empty() || values.is_some() || file.helm == Some(true);
    set.insert(RELEASE_PARAM.to_owned(), file.release.to_string());
    // The secrets' versions it pins, which kiln-base stamps on the pod template (§3.5).
    if !file.secrets.is_empty() {
        set.insert(SECRETS_PARAM.to_owned(), file.secrets.replace(',', " "));
    }
    let helm = |values: Option<Value>| {
        let parameters: Vec<Value> = set.iter().map(|(name, value)| json!({ "name": name, "value": value, "forceString": name == RELEASE_PARAM || name == SECRETS_PARAM || repo::sets_string(file, name) })).collect();
        let mut h = json!({ "releaseName": app.name, "parameters": parameters });
        if let (Some(v), Some(m)) = (values, h.as_object_mut()) {
            m.insert("valuesObject".into(), v);
        }
        h
    };
    let source = match &file.package {
        Package::Platform { provider, .. } => return Err(format!("Argo CD deploys to Kubernetes only, and {} deploys to {provider}: release it in a KilnCD environment", app.name)),
        Package::Helm { repo: chart_repo, chart, chart_version, .. } => {
            let (repo_url, _) = argo_repo(chart_repo);
            let revision = match chart_version {
                Some(v) => repo::fill(v, file)?,
                None => version.clone(),
            };
            json!({ "repoURL": repo_url, "chart": chart, "targetRevision": revision, "helm": helm(values) })
        }
        Package::Git { repo: git_repo, path, .. } => {
            let mut s = json!({ "repoURL": git_repo, "path": path, "targetRevision": version });
            if helm_wanted && let Some(m) = s.as_object_mut() {
                m.insert("helm".into(), helm(values));
            }
            s
        }
    };
    let sync = sync_policy(env, !reg.paused);
    let ns = file.namespace.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| app.namespace().to_owned());
    Ok(json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {
            "name": application_name(app, &env.name),
            "namespace": namespace,
            "labels": { "app.kubernetes.io/managed-by": "kiln", "kiln.dev/app": app.name, "kiln.dev/environment": env.name },
            "annotations": { "kiln.dev/release": file.release.to_string(), "kiln.dev/commit": commit },
        },
        "spec": {
            "project": "default",
            "destination": { "name": cluster_of(env), "namespace": ns },
            "source": source,
            "syncPolicy": sync,
        },
    }))
}

/// The cluster an environment's Applications run in, as Argo CD names it.
pub(crate) fn cluster_of(env: &Environment) -> String {
    env.targets.kubernetes.first().cloned().unwrap_or_else(|| "in-cluster".to_owned())
}

/// The ApplicationSet of an environment.
pub fn set_name(env: &str) -> String {
    format!("kiln-{env}")
}

/// The AppProject of an environment's Applications, which its ApplicationSet names (Argo CD's API
/// takes no ApplicationSet whose project is templated).
pub fn project_name(env: &str) -> String {
    format!("kiln-{env}")
}

/// The label every Application and ApplicationSet kiln makes carries: its stream picks them.
pub const MANAGED: &str = "app.kubernetes.io/managed-by=kiln";

/// Where an Argo CD reads a release repository: kiln's git server, at the address it reaches kiln.
pub fn release_repo_url(kiln_url: &str, repo: &str) -> String {
    format!("{}/git/{repo}.git", kiln_url.trim_end_matches('/'))
}

/// What Argo CD calls a release repository it reads.
pub fn release_repo_name(repo: &str) -> String {
    format!("kiln-{}", repo.replace(['/', '.', '_'], "-"))
}

/// The git files generator's pattern: every registration's `release.json` in the environment.
fn release_files(env: &Environment) -> String {
    match env.release_repo.root.trim_matches('/') {
        "" => format!("*/*/{}/release.json", env.name),
        root => format!("{root}/*/*/{}/release.json", env.name),
    }
}

/// What an environment's ApplicationSet patches its template with for each `release.json` (Go
/// templates over the file's keys, docs/deploy-api.md §6): the package at its version, its values
/// from the release repository (`$release`), what it sets, stringly where the release filled it,
/// and `kiln.release`; then the release repository itself as the `release` source.
const SOURCES: &str = r#"{{- $values := printf "$release/%s/values.yaml" .path.path }}
{{- $raw := dict }}{{ with .package.set }}{{ $raw = . }}{{ end }}
spec:
  sources:
  - repoURL: {{ trimPrefix "oci://" .package.repo | toJson }}
{{- if eq .package.kind "helm" }}
    chart: {{ .package.chart | toJson }}
    targetRevision: {{ .package.chart_version | default .version | toJson }}
{{- else }}
    path: {{ .package.path | default "." | toJson }}
    targetRevision: {{ .version | toJson }}
{{- end }}
{{- if or (eq .package.kind "helm") .helm }}
    helm:
      releaseName: {{ .app | toJson }}
      valueFiles:
      - {{ $values | toJson }}
      ignoreMissingValueFiles: true
      parameters:
      - name: kiln.release
        value: {{ .release | toString | toJson }}
        forceString: true
{{- with .secrets }}
      - name: kiln.secrets
        value: {{ replace "," " " . | toJson }}
        forceString: true
{{- end }}
{{- range $name, $value := .set }}{{ if and (ne $name "kiln.release") (ne $name "kiln.secrets") }}
      - name: {{ $name | toJson }}
        value: {{ $value | toString | toJson }}
        forceString: {{ contains "${{" (get $raw $name | toString) }}
{{- end }}{{ end }}
{{- end }}
  - repoURL: __RELEASE_REPO__
    targetRevision: __BRANCH__
    ref: release
"#;

/// An environment's ApplicationSet: one Application per registration that is enabled and has a
/// version, made from its `release.json` (docs/deploy-api.md §8.3). It creates and updates them,
/// and never deletes one: kiln does, keeping what it runs when asked to. What kiln changes of an
/// Application's sync policy (a pause) stays until kiln changes it back. Put with the refresh
/// annotation, it reads the release repository again at once. In `shadow` (an environment moving
/// to Argo CD, §8.4) its Applications compare and never sync.
pub fn application_set(namespace: &str, env: &Environment, release_url: &str, shadow: bool) -> Value {
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let patch = SOURCES.replace("__RELEASE_REPO__", &quote(release_url)).replace("__BRANCH__", &quote(&env.release_repo.branch));
    let labels = json!({ "app.kubernetes.io/managed-by": "kiln", "kiln.dev/environment": env.name });
    json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "ApplicationSet",
        "metadata": {
            "name": set_name(&env.name),
            "namespace": namespace,
            "labels": labels,
            "annotations": { "argocd.argoproj.io/application-set-refresh": "true" },
        },
        "spec": {
            "goTemplate": true,
            "generators": [{
                "git": { "repoURL": release_url, "revision": env.release_repo.branch, "files": [{ "path": release_files(env) }] },
                "selector": {
                    "matchLabels": { "enabled": "true" },
                    "matchExpressions": [
                        { "key": "version", "operator": "Exists" },
                        { "key": "package.kind", "operator": "NotIn", "values": ["platform"] },
                    ],
                },
            }],
            "syncPolicy": { "applicationsSync": "create-update", "preserveResourcesOnDeletion": true },
            "ignoreApplicationDifferences": [{ "jsonPointers": ["/spec/syncPolicy"] }],
            "template": {
                "metadata": {
                    "name": format!("{}-{}", env.name, "{{ .app }}"),
                    "labels": { "app.kubernetes.io/managed-by": "kiln", "kiln.dev/app": "{{ .app }}", "kiln.dev/environment": env.name },
                    "annotations": { "kiln.dev/release": "{{ .release }}" },
                },
                "spec": {
                    "project": project_name(&env.name),
                    "destination": { "name": cluster_of(env), "namespace": "{{ .namespace | default .app }}" },
                    "syncPolicy": sync_policy(env, !shadow),
                },
            },
            "templatePatch": patch,
        },
    })
}

/// An environment's sync policy for its Applications: automated as it says, or not at all.
fn sync_policy(env: &Environment, automated: bool) -> Value {
    let mut sync = json!({ "syncOptions": ["CreateNamespace=true"] });
    if automated && let Some(m) = sync.as_object_mut() {
        m.insert("automated".into(), json!({ "prune": env.sync.prune, "selfHeal": env.sync.self_heal }));
    }
    sync
}

/// What an environment's AppProject allows: the repositories its apps come from and its release
/// repository, where they run, and the cluster-scoped kinds they make (a Namespace always, for
/// `CreateNamespace`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub sources: BTreeSet<String>,
    /// Cluster and namespace.
    pub destinations: BTreeSet<(String, String)>,
    /// Group and kind.
    pub cluster_kinds: BTreeSet<(String, String)>,
}

impl Allowed {
    /// What a registration of `app` in `env` adds: its package's repository, the environment's
    /// cluster and the app's namespace, its template's cluster-scoped kinds.
    pub fn add(&mut self, app: &App, env: &Environment, release_url: &str, cluster_kinds: &[String]) {
        self.sources.insert(release_url.to_owned());
        if let Some(r) = source_repo(&app.package) {
            self.sources.insert(r);
        }
        self.destinations.insert((cluster_of(env), app.namespace().to_owned()));
        self.cluster_kinds.extend(cluster_kinds.iter().map(|k| cluster_kind(k)));
    }
}

/// The AppProject of an environment's Applications.
pub fn app_project(env: &str, allowed: &Allowed) -> Value {
    let mut kinds = vec![json!({ "group": "", "kind": "Namespace" })];
    kinds.extend(allowed.cluster_kinds.iter().filter(|(g, k)| !(g.is_empty() && k == "Namespace")).map(|(g, k)| json!({ "group": g, "kind": k })));
    json!({
        "metadata": { "name": project_name(env), "labels": { "app.kubernetes.io/managed-by": "kiln", "kiln.dev/environment": env } },
        "spec": {
            "description": format!("kiln's environment {env}: where its apps come from, and where they run"),
            "sourceRepos": allowed.sources,
            "destinations": allowed.destinations.iter().map(|(c, ns)| json!({ "name": c, "namespace": ns })).collect::<Vec<_>>(),
            "clusterResourceWhitelist": kinds,
        },
    })
}

/// A cluster-scoped kind as a template names it (`rbac.authorization.k8s.io/ClusterRole`, or a core
/// kind by its name alone): its group and kind.
pub fn cluster_kind(s: &str) -> (String, String) {
    match s.trim().rsplit_once('/') {
        Some((g, k)) => (g.to_owned(), k.to_owned()),
        None => (String::new(), s.trim().to_owned()),
    }
}

/// A package's repository as Argo CD names it; a platform's isn't Argo CD's.
pub fn source_repo(package: &Package) -> Option<String> {
    match package {
        Package::Helm { repo, .. } => Some(argo_repo(repo).0),
        Package::Git { repo, .. } => Some(repo.clone()),
        Package::Platform { .. } => None,
    }
}

/// An object of an Application, as Argo CD's resource calls name it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl Object {
    /// An object by its group, kind, namespace and name, its API version from what the Application
    /// says it runs (Argo CD asks for it), or the one its group has had since Kubernetes 1.x.
    pub fn of(app: &Value, group: &str, kind: &str, namespace: &str, name: &str) -> Self {
        let field = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let listed = app.pointer("/status/resources").and_then(Value::as_array).into_iter().flatten().find(|r| {
            field(r, "group") == group && field(r, "kind") == kind && field(r, "name") == name && (namespace.is_empty() || field(r, "namespace") == namespace)
        });
        let version = listed.map(|r| field(r, "version")).filter(|v| !v.is_empty()).unwrap_or_else(|| {
            match group {
                "autoscaling" => "v2",
                _ => "v1",
            }
            .to_owned()
        });
        let namespace = listed.map(|r| field(r, "namespace")).filter(|n| !n.is_empty()).unwrap_or_else(|| namespace.to_owned());
        Self { group: group.to_owned(), version, kind: kind.to_owned(), namespace, name: name.to_owned() }
    }
}

/// The `kiln.release` a source carries, if it renders with Helm.
fn release_of(source: &Value) -> Option<String> {
    source.pointer("/helm/parameters")?.as_array()?.iter().find(|p| p.get("name").and_then(Value::as_str) == Some(RELEASE_PARAM))?.get("value")?.as_str().map(str::to_owned)
}

/// The `kiln.release` of what Argo CD compared or synced: its source's, or one of its sources'.
fn release_in(spec: &Value) -> Option<String> {
    spec.get("source").and_then(release_of).or_else(|| spec.get("sources")?.as_array()?.iter().find_map(release_of))
}

/// The package's source of what Argo CD compared or synced: the one that isn't the release
/// repository's.
fn package_source(spec: &Value) -> Option<&Value> {
    spec.get("source").filter(|s| !s.is_null()).or_else(|| spec.get("sources")?.as_array()?.iter().find(|s| s.get("ref").is_none()))
}

/// The commit of the release repository an Application was compared at, where its values come
/// from there: the release has landed only once it holds the release's files.
pub fn release_revision(app: &Value) -> Option<String> {
    let sources = app.pointer("/status/sync/comparedTo/sources")?.as_array()?;
    let i = sources.iter().position(|s| s.get("ref").and_then(Value::as_str) == Some("release"))?;
    app.pointer("/status/sync/revisions")?.as_array()?.get(i)?.as_str().map(str::to_owned)
}

/// The release an Application is for, as its ApplicationSet made it from `release.json` (its
/// annotation, whatever its package).
pub fn made_for(app: &Value) -> Option<i64> {
    app.pointer("/metadata/annotations/kiln.dev~1release")?.as_str()?.parse().ok()
}

/// What an Application says, as kiln shows it: synced and healthy, the release it carries and the
/// one Argo CD compared, the sync under way, and its conditions.
pub fn status(app: &Value) -> Value {
    let s = |p: &str| app.pointer(p).and_then(Value::as_str).unwrap_or("").to_owned();
    let compared = app.pointer("/status/sync/comparedTo").cloned().unwrap_or(Value::Null);
    let op = app.pointer("/status/operationState").cloned().unwrap_or(Value::Null);
    let conditions = app.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default();
    let revisions = app.pointer("/status/sync/revisions").cloned().unwrap_or(Value::Null);
    json!({
        "application": s("/metadata/name"),
        "sync": s("/status/sync/status"),
        "health": s("/status/health/status"),
        "revision": s("/status/sync/revision"),
        "revisions": revisions,
        "release": s("/metadata/annotations/kiln.dev~1release"),
        "compared_release": release_in(&compared),
        "operation": { "phase": op.get("phase").cloned().unwrap_or(Value::Null), "message": op.get("message").cloned().unwrap_or(Value::Null) },
        "reconciled_at": s("/status/reconciledAt"),
        "conditions": conditions.iter().map(|c| json!({ "type": c.get("type"), "message": c.get("message") })).collect::<Vec<_>>(),
    })
}

/// Whether Argo CD compared an Application's spec for a release with what runs: synced or not,
/// it knows how they differ.
pub fn compared(app: &Value, release: i64, version: &str) -> bool {
    let compared = app.pointer("/status/sync/comparedTo").cloned().unwrap_or(Value::Null);
    let ours = match release_in(&compared) {
        Some(r) => r == release.to_string(),
        None => package_source(&compared).and_then(|p| p.get("targetRevision")).and_then(Value::as_str) == Some(version),
    };
    ours && matches!(app.pointer("/status/sync/status").and_then(Value::as_str), Some("Synced" | "OutOfSync"))
}

/// Where a release stands on Argo CD, from its Application (the release's: the caller made sure),
/// and the status to show.
pub fn landed(app: &Value, release: i64, version: &str) -> (Landed, Value) {
    let s = |p: &str| app.pointer(p).and_then(Value::as_str).unwrap_or("").to_owned();
    let compared = app.pointer("/status/sync/comparedTo").cloned().unwrap_or(Value::Null);
    let ours = match release_in(&compared) {
        Some(r) => r == release.to_string(),
        // Plain manifests carry no parameter: the version compared, under our release's spec.
        None => package_source(&compared).and_then(|p| p.get("targetRevision")).and_then(Value::as_str) == Some(version) && s("/metadata/annotations/kiln.dev~1release") == release.to_string(),
    };
    let op = app.pointer("/status/operationState").cloned().unwrap_or(Value::Null);
    let synced_version = |p: &str| op.pointer(p).and_then(Value::as_str) == Some(version);
    let op_ours = op.pointer("/syncResult").and_then(release_in).is_some_and(|r| r == release.to_string())
        || synced_version("/operation/sync/revision")
        || op.pointer("/operation/sync/revisions").and_then(Value::as_array).is_some_and(|rs| rs.iter().any(|r| r.as_str() == Some(version)));
    let phase = op.get("phase").and_then(Value::as_str).unwrap_or("").to_owned();
    // Argo CD can't render the spec: its package isn't there, or doesn't render.
    let refused = app
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| matches!(c.get("type").and_then(Value::as_str), Some("ComparisonError" | "InvalidSpecError")))
        .map(|c| c.get("message").and_then(Value::as_str).unwrap_or("Argo CD can't compare it").to_owned());
    let view = status(app);
    let landed = if op_ours && matches!(phase.as_str(), "Failed" | "Error") {
        Landed::Failed(op.get("message").and_then(Value::as_str).filter(|m| !m.is_empty()).map_or_else(|| format!("Argo CD's sync {}", phase.to_lowercase()), str::to_owned))
    } else if ours && s("/status/sync/status") == "Synced" && s("/status/health/status") == "Healthy" && !matches!(phase.as_str(), "Running" | "Terminating") {
        Landed::Yes
    } else if ours && op_ours && phase == "Succeeded" && s("/status/health/status") == "Degraded" {
        // Synced from this release, and Degraded: what it runs doesn't.
        Landed::Degraded(Some(s("/status/health/message")).filter(|m| !m.is_empty()).unwrap_or_else(|| "Argo CD says its health is Degraded".to_owned()))
    } else if let Some(why) = refused {
        Landed::Refused(why)
    } else {
        Landed::No
    };
    (landed, view)
}

/// The check an integration of kind `argocd` runs (Settings → Integrations → Verify): the version,
/// and whether the token may do what kiln does there: register release repositories, keep
/// AppProjects and ApplicationSets, and create and sync Applications (in kiln's projects, and in
/// the default one where kiln writes them itself).
pub async fn verify(hub: &Hub, name: &str) -> Result<String, String> {
    let argo = Argo::of(hub, name).await?;
    let version = argo.version().await?;
    let wanted = [
        ("repositories", "create", "*"),
        ("projects", "create", "kiln-*"),
        ("projects", "update", "kiln-*"),
        ("applicationsets", "create", "*/kiln-*"),
        ("applicationsets", "update", "*/kiln-*"),
        ("applications", "sync", "kiln-*/*"),
        ("applications", "create", "default/*"),
        ("applications", "sync", "default/*"),
    ];
    let mut missing = Vec::new();
    for (resource, action, sub) in wanted {
        if !argo.can_i(resource, action, sub).await? {
            missing.push(format!("{action} {resource} ({sub})"));
        }
    }
    if missing.is_empty() {
        Ok(format!("Argo CD {version}: the token may register release repositories, keep kiln's projects and ApplicationSets, and sync their Applications"))
    } else {
        Err(format!("Argo CD {version}: the token may not {} (Argo CD's RBAC)", missing.join(", ")))
    }
}

/// What kiln knows of the Applications it made, from each Argo CD's stream of them
/// (`/api/v1/stream/applications`): one stream per instance, started as a release there is first
/// followed and kept up while kiln runs. A release is followed from what its stream last said, and
/// woken as it changes; Argo CD is asked directly only for what its stream doesn't hold (it is
/// down, or just started).
#[derive(Default)]
pub struct Watches {
    streams: Mutex<HashMap<String, Arc<Stream>>>,
}

#[derive(Default)]
struct Stream {
    /// Each Application by name, as the stream last said it.
    apps: Mutex<HashMap<String, Value>>,
    /// The stream is up: what it holds is current.
    live: AtomicBool,
    /// Its integration is gone: it ends.
    stopped: AtomicBool,
}

/// What hears of each Application that changes.
pub type Changed = Arc<dyn Fn(&Value) + Send + Sync>;

impl Watches {
    /// An Application as its Argo CD's stream last said it, while the stream is up.
    pub fn application(&self, integration: &str, name: &str) -> Option<Value> {
        let s = self.streams.lock().get(integration).cloned()?;
        if !s.live.load(Ordering::SeqCst) {
            return None;
        }
        s.apps.lock().get(name).cloned()
    }

    /// Whether an Argo CD's stream is up.
    pub fn live(&self, integration: &str) -> bool {
        self.streams.lock().get(integration).is_some_and(|s| s.live.load(Ordering::SeqCst))
    }

    /// Starts an Argo CD's stream, unless it runs; `changed` hears of each change.
    pub fn ensure(&self, hub: &Arc<Hub>, integration: &str, changed: Changed) {
        let mut streams = self.streams.lock();
        if streams.contains_key(integration) {
            return;
        }
        let s = Arc::new(Stream::default());
        streams.insert(integration.to_owned(), Arc::clone(&s));
        let (hub, integration) = (Arc::clone(hub), integration.to_owned());
        crate::rt::spawn(async move { follow_stream(hub, integration, s, changed).await });
    }

    /// Ends an Argo CD's stream (its integration is gone, or changed): the next look starts it again.
    pub fn forget(&self, integration: &str) {
        if let Some(s) = self.streams.lock().remove(integration) {
            s.stopped.store(true, Ordering::SeqCst);
            s.live.store(false, Ordering::SeqCst);
        }
    }
}

/// How long a stream may say nothing: Argo CD says something of every Application each time it
/// compares it (every three minutes), so a silence this long is a stream that's gone.
const STREAM_IDLE: Duration = Duration::from_secs(300);

async fn follow_stream(hub: Arc<Hub>, integration: String, s: Arc<Stream>, changed: Changed) {
    let mut wait = 1;
    while !hub.shutdown.is_cancelled() && !s.stopped.load(Ordering::SeqCst) {
        match Argo::of(&hub, &integration).await {
            Ok(argo) => match argo.stream(MANAGED).await {
                Ok(mut resp) => {
                    s.apps.lock().clear();
                    s.live.store(true, Ordering::SeqCst);
                    wait = 1;
                    if let Err(why) = read_stream(&hub, &s, &mut resp, &changed).await {
                        tracing::debug!(%integration, %why, "deploy: Argo CD's stream of Applications ended");
                    }
                    s.live.store(false, Ordering::SeqCst);
                }
                Err(why) => tracing::debug!(%integration, %why, "deploy: Argo CD's stream of Applications can't be opened"),
            },
            Err(why) => tracing::debug!(%integration, %why, "deploy: Argo CD can't be reached for its stream"),
        }
        if !hub.shutdown.sleep(Duration::from_secs(wait)).await {
            break;
        }
        wait = (wait * 2).min(60);
    }
    s.live.store(false, Ordering::SeqCst);
}

/// Reads a stream until it ends, keeping each Application it says.
async fn read_stream(hub: &Hub, s: &Stream, resp: &mut crate::http::client::Response, changed: &Changed) -> Result<(), String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut idle = Duration::ZERO;
    loop {
        if hub.shutdown.is_cancelled() || s.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let chunk = match crate::rt::timeout(Duration::from_secs(5), resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => return Err("Argo CD closed it".into()),
            Ok(Err(e)) => return Err(format!("{e:#}")),
            Err(_) => {
                idle += Duration::from_secs(5);
                if idle >= STREAM_IDLE {
                    return Err(format!("nothing for {}s", STREAM_IDLE.as_secs()));
                }
                continue;
            }
        };
        idle = Duration::ZERO;
        buf.extend_from_slice(&chunk);
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            let Some(result) = v.get("result") else { continue };
            let (Some(app), kind) = (result.get("application"), result.get("type").and_then(Value::as_str).unwrap_or("")) else { continue };
            let Some(name) = app.pointer("/metadata/name").and_then(Value::as_str) else { continue };
            if kind == "DELETED" {
                s.apps.lock().remove(name);
            } else {
                s.apps.lock().insert(name.to_owned(), app.clone());
            }
            changed(app);
        }
        if buf.len() > 64 << 20 {
            return Err("a line of more than 64 MiB".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::deploy::model::{Artifact, RepoRef, SCHEMA, Targets};

    fn env() -> Environment {
        Environment {
            name: "production".into(),
            provider: "argocd/prod".into(),
            release_repo: RepoRef::default(),
            targets: Targets { kubernetes: vec!["prod-us".into()], ..Targets::default() },
            gates: Default::default(),
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
            agents: Default::default(),
            argocd: Default::default(),
            owners: Default::default(),
        }
    }

    fn app(package: Package) -> App {
        App { name: "web".into(), project: "shop".into(), owners: vec![], package, namespace: None, properties: BTreeMap::new(), template: None, contacts: Default::default(), source: None, archived: false, registries: None }
    }

    fn helm() -> Package {
        Package::Helm { repo: "oci://ghcr.io/acme/charts".into(), chart: "web".into(), chart_version: None, set: BTreeMap::from([("image.tag".into(), "${{ release.artifacts.image.tag }}".into())]) }
    }

    fn file(package: Package) -> ReleaseFile {
        ReleaseFile {
            schema: SCHEMA,
            project: "shop".into(),
            app: "web".into(),
            environment: "production".into(),
            release: 42,
            enabled: true,
            version: Some("1.4.2".into()),
            package,
            artifacts: BTreeMap::from([("image".into(), Artifact { repository: "ghcr.io/acme/web".into(), tag: "1.4.2".into(), digest: String::new() })]),
            requested_by: "ada".into(),
            at: String::new(),
            rollback_of: None,
            secrets: String::new(),
            namespace: None,
            set: BTreeMap::new(),
            helm: None,
        }
    }

    fn reg() -> Registration {
        Registration { app: "web".into(), environment: "production".into(), enabled: true, paused: false, running: None, created_at: 0, degraded: None }
    }

    #[test]
    fn a_release_becomes_an_application_stamped_with_it() -> Result<(), String> {
        let a = application("argocd", &app(helm()), &env(), &reg(), &file(helm()), Some("replicas: 3\n"), "abc123")?;
        assert_eq!(a.pointer("/metadata/name").and_then(Value::as_str), Some("production-web"));
        assert_eq!(a.pointer("/spec/source/repoURL").and_then(Value::as_str), Some("ghcr.io/acme/charts"), "OCI without its scheme");
        assert_eq!(a.pointer("/spec/source/targetRevision").and_then(Value::as_str), Some("1.4.2"));
        assert_eq!(a.pointer("/spec/source/helm/valuesObject/replicas").and_then(Value::as_i64), Some(3), "the environment's values, inline");
        assert_eq!(release_of(a.pointer("/spec/source").unwrap_or(&Value::Null)).as_deref(), Some("42"));
        assert_eq!(a.pointer("/spec/destination/name").and_then(Value::as_str), Some("prod-us"));
        assert!(a.pointer("/spec/syncPolicy/automated").is_some());
        let paused = Registration { paused: true, ..reg() };
        assert!(application("argocd", &app(helm()), &env(), &paused, &file(helm()), None, "abc")?.pointer("/spec/syncPolicy/automated").is_none(), "paused: no automated sync");
        Ok(())
    }

    #[test]
    fn plain_manifests_carry_no_helm() -> Result<(), String> {
        let git = Package::Git { repo: "https://github.com/acme/web".into(), path: "deploy".into(), set: BTreeMap::new() };
        let a = application("argocd", &app(git.clone()), &env(), &reg(), &file(git), None, "abc")?;
        assert!(a.pointer("/spec/source/helm").is_none());
        assert_eq!(a.pointer("/spec/source/path").and_then(Value::as_str), Some("deploy"));
        Ok(())
    }

    fn argo_app(sync: &str, health: &str, release: &str, phase: &str) -> Value {
        json!({
            "metadata": { "name": "production-web", "annotations": { "kiln.dev/release": release } },
            "status": {
                "sync": { "status": sync, "revision": "1.4.2", "comparedTo": { "source": { "targetRevision": "1.4.2", "helm": { "parameters": [{ "name": "kiln.release", "value": release }] } } } },
                "health": { "status": health },
                "operationState": { "phase": phase, "message": "one or more objects failed to apply", "syncResult": { "source": { "helm": { "parameters": [{ "name": "kiln.release", "value": release }] } } } },
            }
        })
    }

    #[test]
    fn a_release_lands_when_argo_cd_compared_its_spec_and_found_it_synced_and_healthy() {
        assert_eq!(landed(&argo_app("Synced", "Healthy", "42", "Succeeded"), 42, "1.4.2").0, Landed::Yes);
        assert_eq!(landed(&argo_app("Synced", "Healthy", "41", "Succeeded"), 42, "1.4.2").0, Landed::No, "still the release before");
        assert_eq!(landed(&argo_app("OutOfSync", "Healthy", "42", "Running"), 42, "1.4.2").0, Landed::No);
        assert_eq!(landed(&argo_app("Synced", "Progressing", "42", "Succeeded"), 42, "1.4.2").0, Landed::No, "not healthy yet");
        assert!(matches!(landed(&argo_app("OutOfSync", "Degraded", "42", "Failed"), 42, "1.4.2").0, Landed::Failed(m) if m.contains("failed to apply")));
        assert_eq!(landed(&argo_app("OutOfSync", "Degraded", "41", "Failed"), 42, "1.4.2").0, Landed::No, "an earlier release's failure isn't this one's");
        // Synced from this release and Degraded: evidence it doesn't run.
        assert!(matches!(landed(&argo_app("Synced", "Degraded", "42", "Succeeded"), 42, "1.4.2").0, Landed::Degraded(_)));
        assert_eq!(landed(&argo_app("Synced", "Degraded", "41", "Succeeded"), 42, "1.4.2").0, Landed::No, "the release before's health isn't this one's");
    }

    #[test]
    fn an_environment_s_application_set_makes_an_application_of_each_release_json() {
        let mut e = env();
        e.release_repo.root = "deploy".into();
        let url = "http://kiln.internal:7070/git/acme/releases.git";
        let set = application_set("argocd", &e, url, false);
        let at = |p: &str| set.pointer(p).cloned().unwrap_or(Value::Null);
        assert_eq!(at("/metadata/name"), json!("kiln-production"));
        assert_eq!(at("/metadata/annotations/argocd.argoproj.io~1application-set-refresh"), json!("true"), "read again as it's put");
        assert_eq!(at("/spec/generators/0/git/repoURL"), json!(url));
        assert_eq!(at("/spec/generators/0/git/files/0/path"), json!("deploy/*/*/production/release.json"));
        assert_eq!(at("/spec/generators/0/selector/matchLabels/enabled"), json!("true"), "a disabled registration runs nothing");
        assert_eq!(at("/spec/syncPolicy/applicationsSync"), json!("create-update"), "kiln deletes Applications, keeping what they run when asked");
        assert_eq!(at("/spec/syncPolicy/preserveResourcesOnDeletion"), json!(true));
        assert_eq!(at("/spec/ignoreApplicationDifferences/0/jsonPointers/0"), json!("/spec/syncPolicy"), "a pause stays");
        assert_eq!(at("/spec/template/metadata/name"), json!("production-{{ .app }}"), "the name kiln writes Applications under");
        assert_eq!(at("/spec/template/spec/project"), json!("kiln-production"), "its own AppProject, named as it is");
        assert_eq!(at("/spec/template/spec/destination/name"), json!("prod-us"));
        assert_eq!(at("/spec/template/spec/syncPolicy/automated/prune"), json!(true));
        let patch = at("/spec/templatePatch");
        let patch = patch.as_str().unwrap_or_default();
        assert!(patch.contains(&format!("repoURL: \"{url}\"")) && patch.contains("targetRevision: \"main\"") && patch.contains("ref: release"), "{patch}");
        assert!(patch.contains("$release/%s/values.yaml") && patch.contains("kiln.release"), "{patch}");
        assert!(!patch.contains("__"), "every placeholder filled: {patch}");
        // A root of its own is left out where there is none.
        assert_eq!(application_set("argocd", &env(), url, false).pointer("/spec/generators/0/git/files/0/path"), Some(&json!("*/*/production/release.json")));
        assert!(application_set("argocd", &env(), url, true).pointer("/spec/template/spec/syncPolicy/automated").is_none(), "in shadow it compares and never syncs");
    }

    #[test]
    fn an_environment_s_project_allows_where_its_apps_come_from_and_run() {
        let mut allowed = Allowed::default();
        let url = "http://kiln/git/acme/releases.git";
        allowed.add(&app(helm()), &env(), url, &["rbac.authorization.k8s.io/ClusterRole".into(), "Namespace".into()]);
        let git = Package::Git { repo: "https://github.com/acme/api".into(), path: "deploy".into(), set: BTreeMap::new() };
        allowed.add(&App { name: "api".into(), namespace: Some("shop-api".into()), ..app(git) }, &env(), url, &[]);
        let p = app_project("production", &allowed);
        assert_eq!(p.pointer("/metadata/name"), Some(&json!("kiln-production")));
        assert_eq!(p.pointer("/spec/sourceRepos"), Some(&json!(["ghcr.io/acme/charts", url, "https://github.com/acme/api"])));
        assert_eq!(p.pointer("/spec/destinations"), Some(&json!([{ "name": "prod-us", "namespace": "shop-api" }, { "name": "prod-us", "namespace": "web" }])));
        assert_eq!(p.pointer("/spec/clusterResourceWhitelist"), Some(&json!([{ "group": "", "kind": "Namespace" }, { "group": "rbac.authorization.k8s.io", "kind": "ClusterRole" }])), "a Namespace once");
    }

    fn set_app(release: &str, values_at: &str) -> Value {
        let sources = json!([
            { "repoURL": "ghcr.io/acme/charts", "chart": "web", "targetRevision": "1.4.2", "helm": { "parameters": [{ "name": "kiln.release", "value": release }] } },
            { "repoURL": "http://kiln/git/acme/releases.git", "targetRevision": "main", "ref": "release" },
        ]);
        json!({
            "metadata": { "name": "production-web", "annotations": { "kiln.dev/release": release } },
            "status": {
                "sync": { "status": "Synced", "revisions": ["1.4.2", values_at], "comparedTo": { "sources": sources } },
                "health": { "status": "Healthy" },
                "operationState": { "phase": "Succeeded", "syncResult": { "sources": sources } },
            }
        })
    }

    #[test]
    fn a_release_through_an_application_set_is_read_from_its_sources() {
        let a = set_app("42", "abc123");
        assert_eq!(landed(&a, 42, "1.4.2").0, Landed::Yes);
        assert_eq!(landed(&set_app("41", "abc123"), 42, "1.4.2").0, Landed::No, "made from the release before");
        assert_eq!(release_revision(&a).as_deref(), Some("abc123"), "the release repository's commit it was compared at");
        assert_eq!(made_for(&a), Some(42));
        assert_eq!(status(&a).get("compared_release"), Some(&json!("42")));
        assert_eq!(release_revision(&argo_app("Synced", "Healthy", "42", "Succeeded")), None, "one source: values inline");
    }

    #[test]
    fn an_object_s_version_comes_from_what_its_application_runs() {
        let a = json!({ "status": { "resources": [{ "group": "apps", "version": "v1", "kind": "Deployment", "namespace": "web", "name": "web" }, { "group": "autoscaling", "version": "v2", "kind": "HorizontalPodAutoscaler", "namespace": "web", "name": "web" }] } });
        let d = Object::of(&a, "apps", "Deployment", "", "web");
        assert_eq!((d.version.as_str(), d.namespace.as_str()), ("v1", "web"), "its namespace from the Application too");
        assert_eq!(Object::of(&a, "", "Pod", "web", "web-7d9c-abcde").version, "v1", "a pod isn't listed: core's version");
        assert_eq!(cluster_kind("rbac.authorization.k8s.io/ClusterRole"), ("rbac.authorization.k8s.io".to_owned(), "ClusterRole".to_owned()));
        assert_eq!(cluster_kind("Namespace"), (String::new(), "Namespace".to_owned()));
    }

    #[test]
    fn a_spec_argo_cd_cannot_render_is_refused() {
        let mut a = argo_app("Unknown", "Healthy", "41", "Succeeded");
        if let Some(status) = a.get_mut("status").and_then(Value::as_object_mut) {
            status.insert("conditions".into(), json!([{ "type": "ComparisonError", "message": "podinfo:0.0.404: not found" }]));
        }
        assert!(matches!(landed(&a, 42, "0.0.404").0, Landed::Refused(m) if m.contains("not found")));
    }
}
