//! Argo CD as a CD provider (docs/deploy-api.md §8.3), through its REST API with an account's
//! token. An Argo CD instance is an integration of kind `argocd` (Settings → Integrations): its
//! URL, the namespace its Applications live in, a CA to trust when its certificate isn't public,
//! and the token, sealed in kiln's secret store.
//!
//! Each registration is one Application, `<env>-<app>`, which kiln writes on every release: the
//! package at the release's version, the environment's values inline, and `kiln.release=<id>` as a
//! Helm parameter (txst's deployment id). A release has landed when Argo CD compared that very
//! spec (the parameter, or the version for plain manifests), and reports it synced and healthy.
//! Once release repositories are served to readers (ci.md W14), Applications read their values
//! from the release repository instead, and an ApplicationSet per environment makes them.

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

/// An Argo CD instance, reached with its integration's token.
pub struct Argo {
    client: Client,
    base: String,
    token: String,
    /// Where its Applications live.
    pub namespace: String,
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
        Self::new(&base, &token, field("namespace").as_deref(), field("ca").as_deref())
    }

    pub fn new(base: &str, token: &str, namespace: Option<&str>, ca_pem: Option<&str>) -> Result<Self, String> {
        let client = match ca_pem {
            Some(pem) => Client::new(Roots::Pem(pem.as_bytes().to_vec()), None),
            None => Client::public(),
        }
        .map_err(|e| format!("{e:#}"))?;
        Ok(Self { client, base: base.trim_end_matches('/').to_owned(), token: token.to_owned(), namespace: namespace.unwrap_or("argocd").to_owned() })
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

/// The Application a release makes: the package at its version, the environment's values inline,
/// `kiln.release` stamped where Helm renders.
pub fn application(namespace: &str, app: &App, env: &Environment, reg: &Registration, file: &ReleaseFile, values: Option<&str>, commit: &str) -> Result<Value, String> {
    let version = file.version.clone().ok_or("the release names no version")?;
    let mut set = repo::filled_set(&app.package, file)?;
    let raw_set = match &app.package {
        Package::Helm { set, .. } | Package::Git { set, .. } => set,
    };
    let values: Value = match values.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => serde_yaml::from_str::<Value>(v).map_err(|e| format!("values.yaml doesn't read: {e}"))?,
        None => Value::Null,
    };
    let values = if values.as_object().is_some_and(|m| !m.is_empty()) { Some(values) } else { None };
    let helm_wanted = matches!(app.package, Package::Helm { .. }) || !set.is_empty() || values.is_some();
    set.insert(RELEASE_PARAM.to_owned(), file.release.to_string());
    let helm = |values: Option<Value>| {
        let parameters: Vec<Value> = set
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value, "forceString": name == RELEASE_PARAM || raw_set.get(name).is_some_and(|t| t.contains("${{")) }))
            .collect();
        let mut h = json!({ "releaseName": app.name, "parameters": parameters });
        if let (Some(v), Some(m)) = (values, h.as_object_mut()) {
            m.insert("valuesObject".into(), v);
        }
        h
    };
    let source = match &app.package {
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
    let mut sync = json!({ "syncOptions": ["CreateNamespace=true"] });
    if !reg.paused
        && let Some(m) = sync.as_object_mut()
    {
        m.insert("automated".into(), json!({ "prune": env.sync.prune, "selfHeal": env.sync.self_heal }));
    }
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
            "destination": { "name": env.targets.kubernetes.first().cloned().unwrap_or_else(|| "in-cluster".to_owned()), "namespace": app.namespace() },
            "source": source,
            "syncPolicy": sync,
        },
    }))
}

/// The `kiln.release` a compared (or synced) source carries, if it renders with Helm.
fn release_of(source: &Value) -> Option<String> {
    source.pointer("/helm/parameters")?.as_array()?.iter().find(|p| p.get("name").and_then(Value::as_str) == Some(RELEASE_PARAM))?.get("value")?.as_str().map(str::to_owned)
}

/// What an Application says, as kiln shows it: synced and healthy, the release it carries and the
/// one Argo CD compared, the sync under way, and its conditions.
pub fn status(app: &Value) -> Value {
    let s = |p: &str| app.pointer(p).and_then(Value::as_str).unwrap_or("").to_owned();
    let compared = app.pointer("/status/sync/comparedTo/source").cloned().unwrap_or(Value::Null);
    let op = app.pointer("/status/operationState").cloned().unwrap_or(Value::Null);
    let conditions = app.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default();
    json!({
        "application": s("/metadata/name"),
        "sync": s("/status/sync/status"),
        "health": s("/status/health/status"),
        "revision": s("/status/sync/revision"),
        "release": s("/metadata/annotations/kiln.dev~1release"),
        "compared_release": release_of(&compared),
        "operation": { "phase": op.get("phase").cloned().unwrap_or(Value::Null), "message": op.get("message").cloned().unwrap_or(Value::Null) },
        "reconciled_at": s("/status/reconciledAt"),
        "conditions": conditions.iter().map(|c| json!({ "type": c.get("type"), "message": c.get("message") })).collect::<Vec<_>>(),
    })
}

/// Where a release stands on Argo CD, from its Application (the release's: the caller made sure),
/// and the status to show.
pub fn landed(app: &Value, release: i64, version: &str) -> (Landed, Value) {
    let s = |p: &str| app.pointer(p).and_then(Value::as_str).unwrap_or("").to_owned();
    let compared = app.pointer("/status/sync/comparedTo/source").cloned().unwrap_or(Value::Null);
    let ours = match release_of(&compared) {
        Some(r) => r == release.to_string(),
        // Plain manifests carry no parameter: the version compared, under our release's spec.
        None => compared.get("targetRevision").and_then(Value::as_str) == Some(version) && s("/metadata/annotations/kiln.dev~1release") == release.to_string(),
    };
    let op = app.pointer("/status/operationState").cloned().unwrap_or(Value::Null);
    let op_ours = op.pointer("/syncResult/source").map(release_of).is_some_and(|r| r.as_deref() == Some(release.to_string().as_str()))
        || op.pointer("/operation/sync/revision").and_then(Value::as_str) == Some(version);
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
    } else if let Some(why) = refused {
        Landed::Refused(why)
    } else {
        Landed::No
    };
    (landed, view)
}

/// The check an integration of kind `argocd` runs (Settings → Integrations → Verify): the version,
/// and whether the token may create and sync Applications in the default project.
pub async fn verify(hub: &Hub, name: &str) -> Result<String, String> {
    let argo = Argo::of(hub, name).await?;
    let version = argo.version().await?;
    let create = argo.can_i("applications", "create", "default/*").await?;
    let sync = argo.can_i("applications", "sync", "default/*").await?;
    if create && sync {
        Ok(format!("Argo CD {version}: the token may create and sync Applications in the default project"))
    } else {
        Err(format!("Argo CD {version}: the token may not {} Applications in the default project (Argo CD's RBAC)", if create { "sync" } else { "create" }))
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
            targets: Targets { kubernetes: vec!["prod-us".into()] },
            gates: Value::Null,
            sync: Default::default(),
            watch: Default::default(),
            concurrency: Default::default(),
        }
    }

    fn app(package: Package) -> App {
        App { name: "web".into(), project: "shop".into(), owners: vec![], package, namespace: None, properties: BTreeMap::new(), archived: false }
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
        }
    }

    fn reg() -> Registration {
        Registration { app: "web".into(), environment: "production".into(), enabled: true, paused: false, running: None, created_at: 0 }
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
