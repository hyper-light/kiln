//! App definitions in one form ([`AppDef`]), from kiln's `.kiln/deploy.yaml`, from Argo CD
//! `Application` manifests, and from the API.
//!
//! An Argo CD Application keeps every field's meaning: sources (and `ref` sources whose files
//! other sources read as `$values/...`), the Helm, Kustomize, directory and plugin settings, the
//! destination by server URL or name, the sync policy with its options and retry, ignored
//! differences. kiln's own format says the same in fewer words (see `docs/cd.md`): a source can
//! be just a path, an app can deploy `to` environments, and options are kiln's spelling of
//! Argo CD's.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::{AppConfig, IgnoreRule, Retry, SyncOptions, SyncPolicy, Tracking};
use crate::obj;

/// The cluster name of the cluster kiln runs in, and Argo CD's URL for it.
pub const IN_CLUSTER: &str = "in-cluster";
/// Argo CD's server URL for the cluster it runs in.
pub const IN_CLUSTER_URL: &str = "https://kubernetes.default.svc";

/// An application.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct AppDef {
    /// Name, unique in its project.
    pub name: String,
    /// Project.
    pub project: String,
    /// Where its manifests come from.
    pub sources: Vec<Source>,
    /// Where it deploys.
    pub destination: Destination,
    /// How it syncs.
    pub sync: SyncPolicy,
    /// Differences to ignore.
    pub ignore: Vec<IgnoreRule>,
    /// How its objects are recognized.
    pub tracking: Tracking,
    /// The name its objects carry in Argo CD's tracking when it isn't the app's own: the Argo CD
    /// Application it takes over (an environment moving from Argo CD to KilnCD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_name: Option<String>,
    /// Where the definition came from.
    pub origin: Origin,
    /// The environment it deploys to, when it came from one.
    pub environment: Option<String>,
    /// Labels (for selectors in bulk operations and fleets).
    pub labels: BTreeMap<String, String>,
    /// Deployments kept in its history.
    pub history_limit: u32,
    /// Delete its objects when it is deleted (Argo CD's resources finalizer).
    pub cascade: bool,
    /// Labels and annotations for a namespace it creates (`CreateNamespace=true`).
    pub managed_namespace: Option<ManagedNamespace>,
    /// What it deploys that CI built (docs/cd-targets.md §1.3): images by name, resolved for each
    /// revision to a digest, read in its sources as `${{ artifacts.<name> }}`.
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactSpec>,
    /// How a new version takes traffic (docs/cd-targets.md §5): canary, linear or blue/green.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<crate::rollout::RolloutSpec>,
    /// The checks its rollout uses, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, crate::rollout::CheckSpec>,
    /// Its secrets, from kiln's secrets service (docs/deploy-api.md §3.5): one Secret in its
    /// namespace, at exactly the versions named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsRef>,
}

/// An app's secrets as its definition names them: where kiln's secrets service keeps them, the
/// Secret they become, and the version of each a render delivers. Never their values.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsRef {
    /// The service's scope: `app/<app>/<environment>`.
    pub scope: String,
    /// The Secret's name: `<app>-kiln`.
    pub name: String,
    /// Each secret's version, by name.
    pub versions: BTreeMap<String, u32>,
}

impl SecretsRef {
    /// The versions as text: `API_KEY@1,DB_PASSWORD@4`.
    pub fn versions_text(&self) -> String {
        self.versions.iter().map(|(n, v)| format!("{n}@{v}")).collect::<Vec<_>>().join(",")
    }
}

/// The annotation that holds an app's secrets' versions (`API_KEY@1,DB_PASSWORD@4`, never their
/// values) on their Secret and on each pod template that reads it.
pub const SECRETS: &str = "kiln.dev/secrets";

/// The Secret of an app's secrets (docs/deploy-api.md §3.5), each one a key of its data. It goes
/// first (wave -10), before what reads it.
pub fn app_secret<'a>(name: &str, namespace: &str, versions: &str, items: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Value {
    use base64::Engine as _;
    let data: serde_json::Map<String, Value> = items.into_iter().map(|(k, v)| (k.to_owned(), Value::String(base64::engine::general_purpose::STANDARD.encode(v)))).collect();
    let mut annotations = serde_json::Map::new();
    annotations.insert("argocd.argoproj.io/sync-wave".into(), Value::String("-10".into()));
    annotations.insert(SECRETS.into(), Value::String(versions.to_owned()));
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": namespace, "annotations": annotations },
        "type": "Opaque",
        "data": data,
    })
}

/// Marks each pod template in `namespace` that reads Secret `name` (as env, or as files) with its
/// `versions`: Kubernetes doesn't restart pods when a Secret they read changes, a new template
/// does, so a new version rolls them.
pub fn stamp_secrets(objects: &mut [Value], name: &str, namespace: &str, versions: &str) {
    for o in objects.iter_mut() {
        let template: &[&str] = match (obj::group(o), obj::kind(o)) {
            ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch", "Job") => &["spec", "template"],
            ("batch", "CronJob") => &["spec", "jobTemplate", "spec", "template"],
            ("", "Pod") => &[],
            _ => continue,
        };
        if !(obj::namespace(o).is_empty() || obj::namespace(o) == namespace) {
            continue;
        }
        let pod: Vec<&str> = template.iter().copied().chain(["spec"]).collect();
        if !obj::get(o, &pod).is_some_and(|p| reads_secret(p, name)) {
            continue;
        }
        let at: Vec<&str> = template.iter().copied().chain(["metadata", "annotations"]).collect();
        if let Some(ann) = obj::ensure_map(o, &at) {
            ann.insert(SECRETS.into(), Value::String(versions.to_owned()));
        }
    }
}

/// Whether a pod's spec reads Secret `name`: a container's env (all of it, or a key) or a volume
/// (of its own, or projected).
fn reads_secret(pod: &Value, name: &str) -> bool {
    let named = |v: Option<&Value>| v.and_then(Value::as_str) == Some(name);
    let list = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let containers = ["containers", "initContainers", "ephemeralContainers"].iter().flat_map(|k| list(pod, k)).collect::<Vec<_>>();
    let env = containers.iter().any(|c| {
        list(c, "envFrom").iter().any(|e| named(e.pointer("/secretRef/name"))) || list(c, "env").iter().any(|e| named(e.pointer("/valueFrom/secretKeyRef/name")))
    });
    let files = list(pod, "volumes").iter().any(|v| {
        named(v.pointer("/secret/secretName")) || v.pointer("/projected/sources").and_then(Value::as_array).into_iter().flatten().any(|s| named(s.pointer("/secret/name")))
    });
    env || files
}

/// The annotation a traffic-carrying object holds its app's rollout in: `{ rollout, checks }`.
pub const ROLLOUT: &str = "kiln.dev/rollout";

/// Marks the objects that carry traffic (a Lambda alias, a Worker, a production deployment)
/// with the app's rollout, so the provider that applies them rolls them out.
pub fn stamp_rollout(def: &AppDef, objects: &mut [Value]) {
    let Some(r) = &def.rollout else { return };
    let text = serde_json::json!({ "rollout": r, "checks": def.checks }).to_string();
    for o in objects.iter_mut() {
        let traffic = crate::providers::kind_def(obj::group(o), obj::kind(o)).is_some_and(|k| k.traffic);
        if traffic && let Some(ann) = obj::ensure_map(o, &["metadata", "annotations"]) {
            ann.insert(ROLLOUT.into(), Value::String(text.clone()));
        }
    }
}

/// An artifact an app deploys: an image, resolved for each revision to an immutable digest
/// (docs/cd-targets.md §1.3, §7). Its fields may read `${{ git.sha }}` and `${{ git.short_sha }}`:
/// they are the app's revision when it resolves, so a rollback deploys that revision's image.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ArtifactSpec {
    /// The image: `docker.io/acme/kiln:${{ git.sha }}`, or a name with `tag` below.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// Instead of `image`: an image kiln publishes under this name, built by `pipeline` at each
    /// commit. Where it lives (the local Docker that builds it, loaded into the cluster's nodes,
    /// or a registry) is a server setting (Settings → Deployments → Images).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// The pipeline that builds a published image (default `image`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    /// Instead of an image: a file a workflow's run at the commit uploaded (`artifact`, its
    /// name), from `workflow` (the workflow's name, or its file). It resolves to its content's
    /// SHA-256 (`sha256:<hex>`): a zip, a bundle, a built site (docs/cd-targets.md §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The artifact's name, with `workflow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Its tag, when `image` carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The registry integration it is pulled with (by default the one set up for its registry, or
    /// none: anonymously).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// A Secret (`kubernetes.io/dockerconfigjson`) kiln keeps beside the app with the registry's
    /// credentials, to name in `imagePullSecrets`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_secret: Option<String>,
    /// Platforms its index must hold (`linux/amd64`): a revision whose image lacks one waits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

impl ArtifactSpec {
    /// The reference to resolve for `revision` (its `git.*` filled in).
    pub fn reference(&self, revision: &str) -> String {
        let mut scope = Scope::default();
        scope.set("git.sha", Value::String(revision.to_owned()));
        scope.set("git.short_sha", Value::String(revision.chars().take(7).collect()));
        let image = scope.interpolate(&self.image);
        match self.tag.as_deref().map(|t| scope.interpolate(t)).filter(|t| !t.is_empty()) {
            Some(t) => format!("{image}:{t}"),
            None => image,
        }
    }
}

/// An artifact as one revision resolved it.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Its name in the app.
    pub name: String,
    /// The image pinned: `docker.io/acme/kiln@sha256:…`.
    pub reference: String,
    /// `docker.io/acme/kiln`.
    pub repository: String,
    /// The tag it was resolved from.
    #[serde(default)]
    pub tag: Option<String>,
    /// `sha256:…`.
    pub digest: String,
}

/// Replaces `${{ artifacts.<name> }}` (the pinned reference) and `${{ artifacts.<name>.digest }}`,
/// `.repository` and `.tag` in `text`. A name no artifact has is an error, so a typo can't deploy
/// an empty image.
pub fn substitute_artifacts(text: &str, artifacts: &[ArtifactRef]) -> Result<String, DefError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        out.push_str(rest.get(..start).unwrap_or(""));
        let after = rest.get(start + 3..).unwrap_or("");
        let Some(end) = after.find("}}") else {
            out.push_str(rest.get(start..).unwrap_or(""));
            return Ok(out);
        };
        let expr = after.get(..end).unwrap_or("").trim();
        match expr.strip_prefix("artifacts.") {
            Some(path) => {
                let (name, field) = path.split_once('.').unwrap_or((path, ""));
                let a = artifacts.iter().find(|a| a.name == name).ok_or_else(|| DefError(format!("`${{{{ {expr} }}}}`: the app has no artifact `{name}`")))?;
                match field {
                    "" | "reference" => out.push_str(&a.reference),
                    "digest" => out.push_str(&a.digest),
                    "repository" => out.push_str(&a.repository),
                    "tag" => out.push_str(a.tag.as_deref().unwrap_or("")),
                    other => return Err(DefError(format!("`${{{{ {expr} }}}}`: an artifact has no `{other}` (reference, digest, repository, tag)"))),
                }
            }
            None => {
                out.push_str("${{");
                out.push_str(after.get(..end).unwrap_or(""));
                out.push_str("}}");
            }
        }
        rest = after.get(end + 2..).unwrap_or("");
    }
    out.push_str(rest);
    Ok(out)
}

/// Fills `${{ artifacts.* }}` in every string of `v` (a rendered object, say).
pub fn substitute_value(v: &mut Value, artifacts: &[ArtifactRef]) -> Result<(), DefError> {
    match v {
        Value::String(s) if s.contains("${{") => *s = substitute_artifacts(s, artifacts)?,
        Value::Array(a) => {
            for x in a {
                substitute_value(x, artifacts)?;
            }
        }
        Value::Object(m) => {
            for x in m.values_mut() {
                substitute_value(x, artifacts)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// An app's sources with its artifacts filled in, as a revision resolved them.
pub fn with_artifacts(sources: &[Source], artifacts: &[ArtifactRef]) -> Result<Vec<Source>, DefError> {
    let mut v = serde_json::to_value(sources).map_err(|e| DefError(format!("sources: {e}")))?;
    substitute_value(&mut v, artifacts)?;
    serde_json::from_value(v).map_err(|e| DefError(format!("sources: {e}")))
}

/// The Secret an artifact's `pull-secret` names: the registry's credentials as a Docker config.
/// It goes first (wave -10), before anything that pulls with it.
pub fn pull_secret(name: &str, namespace: &str, docker_config: &str) -> Value {
    use base64::Engine as _;
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": namespace, "annotations": { "argocd.argoproj.io/sync-wave": "-10" } },
        "type": "kubernetes.io/dockerconfigjson",
        "data": { ".dockerconfigjson": base64::engine::general_purpose::STANDARD.encode(docker_config) },
    })
}

/// Labels and annotations for a created namespace.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct ManagedNamespace {
    /// Labels.
    pub labels: BTreeMap<String, String>,
    /// Annotations.
    pub annotations: BTreeMap<String, String>,
}

impl AppDef {
    /// The app's id: `project/name`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.project, self.name)
    }

    /// What the engine needs of it.
    pub fn config(&self) -> AppConfig {
        let mut c = AppConfig::new(&self.id(), &self.destination.cluster, &self.destination.namespace);
        c.project = self.project.as_str().into();
        c.sync = self.sync;
        c.ignore = self.ignore.clone();
        c.tracking = self.tracking;
        if self.tracking != Tracking::Kiln {
            c.tracking_name = Some(self.tracking_name.as_deref().unwrap_or(&self.name).into());
            // Adopted from Argo CD: apply as Argo CD's own field manager, so what Argo CD owned
            // stays owned (and removable) after the take-over.
            c.manager = "argocd-controller".into();
        }
        if self.sync.options.create_namespace == Some(true) {
            let m = self.managed_namespace.clone().unwrap_or_default();
            c.create_namespace = Some((m.labels.into_iter().collect(), m.annotations.into_iter().collect()));
            c.manage_namespace = self.managed_namespace.is_some();
        }
        c
    }
}

/// Where an app deploys.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Destination {
    /// The cluster, by kiln's name for it.
    pub cluster: String,
    /// The namespace for namespaced objects that don't name one.
    pub namespace: String,
}

/// Where a definition came from.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Origin {
    /// A repository's `.kiln/deploy.yaml`.
    Deploy {
        /// The repository.
        repo: String,
    },
    /// An Argo CD Application: rendered by another app (`parent`), or imported.
    Argo {
        /// The app that rendered it, if any.
        parent: Option<String>,
    },
    /// Created through the API.
    Api,
}

/// One source of an app's manifests.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Source {
    /// Repository URL (git, Helm or OCI), or a kiln repository name.
    pub repo: String,
    /// Path in the repository (git sources).
    pub path: String,
    /// Branch, tag, commit, or for charts a version constraint.
    pub revision: String,
    /// A Helm chart's name, for charts from Helm repositories.
    pub chart: Option<String>,
    /// The name other sources read this one's files by (`$name/...`), for sources that only
    /// provide files.
    pub as_ref: Option<String>,
    /// How to render it.
    pub engine: Engine,
}

/// How a source is rendered.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum Engine {
    /// Detect from the files: `Chart.yaml` is Helm, a kustomization is Kustomize, else a
    /// directory.
    #[default]
    Auto,
    /// Plain YAML and JSON.
    Directory(Directory),
    /// A Helm chart.
    Helm(Helm),
    /// A Kustomize build.
    Kustomize(Kustomize),
    /// A config management plugin.
    Plugin(Plugin),
    /// A provider's definition (docs/cd-targets.md §4): the provider renders its objects.
    Provider(ProviderSource),
}

/// An app's definition for a provider: its key in `.kiln/deploy.yaml`, and its target's other
/// settings.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ProviderSource {
    /// The provider (`aws-ecs`).
    pub provider: String,
    /// The definition: its key's value.
    pub spec: Value,
    /// The target's settings beyond its account and place (`worker-suffix`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub target: Map<String, Value>,
}

/// Directory settings.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Directory {
    /// Read subdirectories too.
    pub recurse: bool,
    /// Only files matching this glob (`{a,b}` alternatives allowed).
    pub include: Option<String>,
    /// Never files matching this glob.
    pub exclude: Option<String>,
}

/// One `--set`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct HelmParam {
    /// The value's path (`image.tag`).
    pub name: String,
    /// The value.
    pub value: String,
    /// As a string, whatever it looks like (`--set-string`).
    pub force_string: bool,
}

/// Helm settings.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Helm {
    /// Values files, relative to the chart, or `$ref/path` in another source.
    pub value_files: Vec<String>,
    /// Values given inline (Argo CD's `values` or `valuesObject`).
    pub values: Option<Value>,
    /// `--set` and `--set-string` values.
    pub parameters: Vec<HelmParam>,
    /// `--set-file` values: (name, path).
    pub file_parameters: Vec<(String, String)>,
    /// Release name; the app's name if unset.
    pub release_name: Option<String>,
    /// Namespace given to the chart; the destination's if unset.
    pub namespace: Option<String>,
    /// Leave out CRDs.
    pub skip_crds: bool,
    /// Leave out test hooks.
    pub skip_tests: bool,
    /// Skip the chart's values schema.
    pub skip_schema_validation: bool,
    /// Missing values files are not an error.
    pub ignore_missing_value_files: bool,
    /// The Kubernetes version the chart sees, if not the destination's.
    pub kube_version: Option<String>,
    /// The API versions the chart sees, if not the destination's.
    pub api_versions: Vec<String>,
}

/// Kustomize settings (Argo CD's overrides of the kustomization).
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Kustomize {
    /// Image overrides (`name=newName:tag`, `name:tag`).
    pub images: Vec<String>,
    /// Prefix for every name.
    pub name_prefix: Option<String>,
    /// Suffix for every name.
    pub name_suffix: Option<String>,
    /// Labels for every object.
    pub common_labels: BTreeMap<String, String>,
    /// Annotations for every object.
    pub common_annotations: BTreeMap<String, String>,
    /// Namespace for every namespaced object.
    pub namespace: Option<String>,
    /// Replica overrides: (name, count).
    pub replicas: Vec<(String, i64)>,
    /// Patches, as kustomization `patches` entries.
    pub patches: Vec<Value>,
    /// Components to add.
    pub components: Vec<String>,
}

/// Plugin settings.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Plugin {
    /// The plugin's name.
    pub name: String,
    /// Environment for its commands (Argo CD prefixes them with `ARGOCD_ENV_`).
    pub env: Vec<(String, String)>,
    /// Parameters, as Argo CD passes them (`ARGOCD_APP_PARAMETERS`).
    pub parameters: Vec<Value>,
}

/// A definition kiln could not read, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefError(pub String);

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

fn string_map(v: &Value, k: &str) -> BTreeMap<String, String> {
    v.get(k)
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))).collect())
        .unwrap_or_default()
}

/// A Go duration (`5s`, `3m`, `1h30m`, `250ms`) in milliseconds.
pub fn duration_ms(text: &str) -> Option<u64> {
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = text.trim().chars().peekable();
    if text.trim().is_empty() {
        return None;
    }
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        let unit = match c {
            'h' => 3_600_000.0,
            'm' if chars.peek() == Some(&'s') => {
                chars.next();
                1.0
            }
            'm' => 60_000.0,
            's' => 1_000.0,
            'u' | 'µ' if chars.peek() == Some(&'s') => {
                chars.next();
                0.001
            }
            'n' if chars.peek() == Some(&'s') => {
                chars.next();
                0.000_001
            }
            _ => return None,
        };
        total += n * unit;
    }
    if !num.is_empty() {
        // A bare number: seconds, as Argo CD reads them.
        total += num.parse::<f64>().ok()? * 1000.0;
    }
    Some(total as u64)
}

fn retry_of(v: &Value) -> Option<Retry> {
    let r = v.as_object()?;
    let mut out = Retry::default();
    if let Some(l) = r.get("limit").and_then(obj::as_i64) {
        out.limit = l;
    }
    if let Some(bo) = r.get("backoff") {
        if let Some(d) = s(bo, "duration").and_then(|d| duration_ms(&d)) {
            out.backoff_ms = d;
        }
        if let Some(f) = bo.get("factor").and_then(obj::as_i64) {
            out.factor = f.max(1) as u64;
        }
        if let Some(m) = s(bo, "maxDuration").and_then(|d| duration_ms(&d)) {
            out.max_ms = m;
        }
    }
    Some(out)
}

fn ignore_of(v: &Value) -> Vec<IgnoreRule> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| IgnoreRule {
                    group: s(i, "group").unwrap_or_default(),
                    kind: s(i, "kind").unwrap_or_default(),
                    name: s(i, "name"),
                    namespace: s(i, "namespace"),
                    json_pointers: strings(i, "jsonPointers"),
                    jq_paths: strings(i, "jqPathExpressions"),
                    managers: strings(i, "managedFieldsManagers"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn argo_source(v: &Value) -> Source {
    let engine = if let Some(h) = v.get("helm") {
        let mut values = h.get("valuesObject").cloned().filter(|o| o.is_object());
        if values.is_none()
            && let Some(text) = s(h, "values").filter(|t| !t.trim().is_empty()) {
                values = serde_yaml::from_str::<Value>(&text).ok();
            }
        Engine::Helm(Helm {
            value_files: strings(h, "valueFiles"),
            values,
            parameters: h
                .get("parameters")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .map(|p| HelmParam {
                            name: s(p, "name").unwrap_or_default(),
                            value: s(p, "value").unwrap_or_default(),
                            force_string: b(p, "forceString"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            file_parameters: h
                .get("fileParameters")
                .and_then(Value::as_array)
                .map(|ps| ps.iter().map(|p| (s(p, "name").unwrap_or_default(), s(p, "path").unwrap_or_default())).collect())
                .unwrap_or_default(),
            release_name: s(h, "releaseName"),
            namespace: s(h, "namespace"),
            skip_crds: b(h, "skipCrds"),
            skip_tests: b(h, "skipTests"),
            skip_schema_validation: b(h, "skipSchemaValidation"),
            ignore_missing_value_files: b(h, "ignoreMissingValueFiles"),
            kube_version: s(h, "kubeVersion"),
            api_versions: strings(h, "apiVersions"),
        })
    } else if let Some(k) = v.get("kustomize") {
        Engine::Kustomize(Kustomize {
            images: strings(k, "images"),
            name_prefix: s(k, "namePrefix"),
            name_suffix: s(k, "nameSuffix"),
            common_labels: string_map(k, "commonLabels"),
            common_annotations: string_map(k, "commonAnnotations"),
            namespace: s(k, "namespace"),
            replicas: k
                .get("replicas")
                .and_then(Value::as_array)
                .map(|rs| rs.iter().map(|r| (s(r, "name").unwrap_or_default(), r.get("count").and_then(obj::as_i64).unwrap_or(1))).collect())
                .unwrap_or_default(),
            patches: k.get("patches").and_then(Value::as_array).cloned().unwrap_or_default(),
            components: strings(k, "components"),
        })
    } else if let Some(d) = v.get("directory") {
        Engine::Directory(Directory { recurse: b(d, "recurse"), include: s(d, "include"), exclude: s(d, "exclude") })
    } else if let Some(p) = v.get("plugin") {
        Engine::Plugin(Plugin {
            name: s(p, "name").unwrap_or_default(),
            env: p
                .get("env")
                .and_then(Value::as_array)
                .map(|es| es.iter().map(|e| (s(e, "name").unwrap_or_default(), s(e, "value").unwrap_or_default())).collect())
                .unwrap_or_default(),
            parameters: p.get("parameters").and_then(Value::as_array).cloned().unwrap_or_default(),
        })
    } else {
        Engine::Auto
    };
    Source {
        repo: s(v, "repoURL").unwrap_or_default(),
        path: s(v, "path").unwrap_or_default(),
        revision: s(v, "targetRevision").filter(|r| !r.is_empty()).unwrap_or_else(|| "HEAD".into()),
        chart: s(v, "chart"),
        as_ref: s(v, "ref"),
        engine,
    }
}

/// Reads an Argo CD `Application`. `clusters` maps server URLs to kiln cluster names.
pub fn from_argo(v: &Value, parent: Option<&str>, clusters: &dyn Fn(&str) -> Option<String>) -> Result<AppDef, DefError> {
    if obj::kind(v) != "Application" || obj::group(v) != "argoproj.io" {
        return Err(DefError(format!("not an Argo CD Application: {}/{}", obj::group(v), obj::kind(v))));
    }
    let name = obj::name(v).to_owned();
    if name.is_empty() {
        return Err(DefError("Application has no name".into()));
    }
    let spec = v.get("spec").ok_or_else(|| DefError(format!("Application {name} has no spec")))?;
    let mut sources: Vec<Source> = match spec.get("sources").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().map(argo_source).collect(),
        _ => spec.get("source").map(argo_source).into_iter().collect(),
    };
    if sources.is_empty() {
        return Err(DefError(format!("Application {name} has no source")));
    }
    for src in &mut sources {
        src.repo = src.repo.trim().to_owned();
    }
    let dest = spec.get("destination").cloned().unwrap_or(Value::Null);
    let cluster = match (s(&dest, "name"), s(&dest, "server")) {
        (Some(n), _) if !n.is_empty() => n,
        (_, Some(url)) if url.trim_end_matches('/') == IN_CLUSTER_URL => IN_CLUSTER.into(),
        (_, Some(url)) => clusters(&url).ok_or_else(|| DefError(format!("Application {name}: no cluster is registered at {url}")))?,
        _ => IN_CLUSTER.into(),
    };
    let policy = spec.get("syncPolicy").cloned().unwrap_or(Value::Null);
    let automated = policy.get("automated");
    let (options, _) = SyncOptions::parse(strings(&policy, "syncOptions").iter().map(String::as_str));
    let sync = SyncPolicy {
        auto: automated.is_some_and(|a| a.get("enabled").and_then(Value::as_bool).unwrap_or(true)),
        prune: automated.is_some_and(|a| b(a, "prune")),
        self_heal: automated.is_some_and(|a| b(a, "selfHeal")),
        allow_empty: automated.is_some_and(|a| b(a, "allowEmpty")),
        options,
        retry: policy.get("retry").and_then(retry_of),
    };
    let finalizers = obj::list_at(v, &["metadata", "finalizers"]).cloned().unwrap_or_default();
    let cascade = finalizers.iter().filter_map(Value::as_str).any(|f| f.starts_with("resources-finalizer.argocd.argoproj.io"));
    let managed_namespace = policy.get("managedNamespaceMetadata").map(|m| ManagedNamespace {
        labels: string_map(m, "labels"),
        annotations: string_map(m, "annotations"),
    });
    Ok(AppDef {
        name,
        project: s(spec, "project").filter(|p| !p.is_empty()).unwrap_or_else(|| "default".into()),
        sources,
        destination: Destination { cluster, namespace: s(&dest, "namespace").unwrap_or_default() },
        sync,
        ignore: spec.get("ignoreDifferences").map(ignore_of).unwrap_or_default(),
        tracking: Tracking::ArgoAnnotation,
        tracking_name: None,
        origin: Origin::Argo { parent: parent.map(str::to_owned) },
        environment: None,
        labels: obj::map_at(v, &["metadata", "labels"])
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
            .unwrap_or_default(),
        history_limit: spec.get("revisionHistoryLimit").and_then(obj::as_i64).map(|n| n.clamp(0, 1000) as u32).unwrap_or(10),
        cascade,
        managed_namespace,
        artifacts: BTreeMap::new(),
        rollout: None,
        checks: BTreeMap::new(),
        secrets: None,
    })
}

/// kiln's deploy file (`.kiln/deploy.yaml`), as written.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployFile {
    /// Apps by name, as written: each an [`AppSpec`], plus at most one provider's key
    /// (`aws-ecs:`), split off by [`split_provider`].
    #[serde(default)]
    pub apps: BTreeMap<String, Value>,
    /// Checks every app's rollout may use, by name (docs/cd-targets.md §5).
    #[serde(default)]
    pub checks: BTreeMap<String, Value>,
}

/// An app's settings and its provider's key (`aws-ecs: {…}`), if it names one.
pub fn split_provider(name: &str, raw: &Value) -> Result<(AppSpec, Option<(String, Value)>), DefError> {
    let Value::Object(m) = raw else { return Err(DefError(format!("app {name}: a map of settings"))) };
    let mut rest = m.clone();
    let named: Vec<String> = m.keys().filter(|k| crate::providers::get(k).is_some()).cloned().collect();
    if named.len() > 1 {
        return Err(DefError(format!("app {name} names {}: one provider per app", named.join(" and "))));
    }
    let provider = named.first().map(|p| (p.clone(), rest.remove(p).unwrap_or(Value::Null)));
    let spec: AppSpec = serde_json::from_value(Value::Object(rest)).map_err(|e| DefError(format!("app {name}: {e}")))?;
    Ok((spec, provider))
}

/// One app in `.kiln/deploy.yaml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct AppSpec {
    /// A path in this repository, or a source.
    #[serde(default)]
    pub source: Option<SourceSpec>,
    /// Several sources.
    #[serde(default)]
    pub sources: Vec<SourceSpec>,
    /// Helm settings for the (first) chart source.
    #[serde(default)]
    pub helm: Option<HelmSpec>,
    /// Kustomize settings.
    #[serde(default)]
    pub kustomize: Option<Value>,
    /// Directory settings.
    #[serde(default)]
    pub directory: Option<Directory>,
    /// Plugin settings.
    #[serde(default)]
    pub plugin: Option<Value>,
    /// Environments, or a cluster and namespace.
    #[serde(default)]
    pub to: Option<ToSpec>,
    /// Default namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Project.
    #[serde(default)]
    pub project: Option<String>,
    /// Sync policy.
    #[serde(default)]
    pub sync: Option<SyncSpec>,
    /// Differences to ignore.
    #[serde(default)]
    pub ignore: Vec<IgnoreSpec>,
    /// Labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Artifacts it deploys.
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactSpec>,
    /// How a new version takes traffic.
    #[serde(default)]
    pub rollout: Option<crate::rollout::RolloutSpec>,
    /// Checks of its own, beside the file's.
    #[serde(default)]
    pub checks: BTreeMap<String, Value>,
}

/// A source: a path, or more.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum SourceSpec {
    /// A path in this repository.
    Path(String),
    /// A full source.
    Full {
        /// Repository (this one if unset).
        #[serde(default)]
        repo: Option<String>,
        /// Path.
        #[serde(default)]
        path: Option<String>,
        /// Revision (trunk if unset).
        #[serde(default, alias = "revision")]
        r#ref: Option<String>,
        /// Chart name (Helm repositories).
        #[serde(default)]
        chart: Option<String>,
        /// Chart version.
        #[serde(default)]
        version: Option<String>,
        /// The name other sources read its files by.
        #[serde(default, rename = "as")]
        as_ref: Option<String>,
    },
}

/// Helm settings in kiln's spelling.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct HelmSpec {
    /// Values files.
    #[serde(default)]
    pub values: Vec<String>,
    /// Inline values.
    #[serde(default)]
    pub values_object: Option<Value>,
    /// `--set` values.
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
    /// `--set-string` values.
    #[serde(default)]
    pub set_string: BTreeMap<String, String>,
    /// `--set-file` values.
    #[serde(default)]
    pub set_file: BTreeMap<String, String>,
    /// Release name.
    #[serde(default)]
    pub release_name: Option<String>,
    /// Leave out CRDs.
    #[serde(default)]
    pub skip_crds: bool,
    /// Leave out tests.
    #[serde(default)]
    pub skip_tests: bool,
}

/// Where an app deploys.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ToSpec {
    /// One environment.
    One(String),
    /// Several environments.
    Many(Vec<String>),
    /// A cluster and namespace.
    Place {
        /// Cluster name.
        cluster: String,
        /// Namespace.
        #[serde(default)]
        namespace: Option<String>,
    },
}

/// Sync policy in kiln's spelling.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SyncSpec {
    /// Sync when git moves.
    #[serde(default)]
    pub auto: bool,
    /// Automatic syncs prune.
    #[serde(default)]
    pub prune: bool,
    /// Heal drift.
    #[serde(default)]
    pub self_heal: bool,
    /// Allow syncing to nothing.
    #[serde(default)]
    pub allow_empty: bool,
    /// Argo CD sync options.
    #[serde(default)]
    pub options: Vec<String>,
    /// Retries.
    #[serde(default)]
    pub retry: Option<RetrySpec>,
}

/// Retries in kiln's spelling.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct RetrySpec {
    /// Attempts after the first.
    #[serde(default = "default_limit")]
    pub limit: i64,
    /// First wait (`5s`).
    #[serde(default)]
    pub backoff: Option<String>,
    /// Growth factor.
    #[serde(default)]
    pub factor: Option<u64>,
    /// Longest wait (`3m`).
    #[serde(default)]
    pub max: Option<String>,
}

fn default_limit() -> i64 {
    5
}

/// An ignore rule in kiln's spelling.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct IgnoreSpec {
    /// Group.
    #[serde(default)]
    pub group: String,
    /// Kind.
    #[serde(default)]
    pub kind: String,
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// JSON pointers.
    #[serde(default, alias = "json-pointers")]
    pub pointers: Vec<String>,
    /// jq paths.
    #[serde(default)]
    pub jq: Vec<String>,
    /// Field managers.
    #[serde(default)]
    pub managers: Vec<String>,
}

/// An environment as `.kiln/environments.yaml` describes it, the parts deployments read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct EnvironmentSpec {
    /// Its clusters.
    #[serde(default)]
    pub clusters: Vec<String>,
    /// Its targets beyond clusters, by provider (docs/cd-targets.md §4):
    /// `aws-ecs: { account: aws-prod, region: us-east-1 }`.
    #[serde(default)]
    pub targets: BTreeMap<String, Map<String, Value>>,
    /// The environment it follows.
    #[serde(default)]
    pub after: Option<String>,
    /// How long a revision must run healthy in `after` first.
    #[serde(default)]
    pub soak: Option<String>,
    /// What agents may do in it (docs/cd-targets.md §6): nothing that changes what runs, unless
    /// people say so.
    #[serde(default)]
    pub agents: AgentRights,
}

/// What agents may do in an environment. Off unless said (docs/cd-targets.md, D6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AgentRights {
    /// Sync, roll back, take over and delete its apps.
    #[serde(default)]
    pub deploy: bool,
    /// Approve what waits for approval there (ci.md W15's `agents-can-approve`).
    #[serde(default)]
    pub approve: bool,
}

/// What `${{ }}` in a deploy file can read.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// The values, as nested objects: `env.name`, `cluster.labels.region`, `git.sha`.
    pub values: Map<String, Value>,
}

impl Scope {
    /// Sets `path` (dotted) to `value`.
    pub fn set(&mut self, path: &str, value: Value) {
        let mut parts = path.split('.').peekable();
        let mut cur = &mut self.values;
        while let Some(p) = parts.next() {
            if parts.peek().is_none() {
                cur.insert(p.to_owned(), value);
                return;
            }
            let next = cur.entry(p.to_owned()).or_insert_with(|| Value::Object(Map::new()));
            if !next.is_object() {
                *next = Value::Object(Map::new());
            }
            let Some(m) = next.as_object_mut() else { return };
            cur = m;
        }
    }

    fn lookup(&self, path: &str) -> Option<&Value> {
        let mut parts = path.trim().split('.');
        let first = parts.next()?;
        let mut cur = self.values.get(first)?;
        for p in parts {
            cur = cur.get(p)?;
        }
        Some(cur)
    }

    /// Replaces every `${{ path }}` in `text`; unknown names are empty, as in pipelines. Only
    /// `artifacts.*` is kept as written: it is known when a revision resolves, not when the file is
    /// read.
    pub fn interpolate(&self, text: &str) -> String {
        self.interpolate_keeping(text, &["artifacts"])
    }

    /// Every string in `v` interpolated, keeping `${{ }}` whose first name is in `keep`.
    pub fn interpolate_value_keeping(&self, v: &Value, keep: &[&str]) -> Value {
        match v {
            Value::String(t) => Value::String(self.interpolate_keeping(t, keep)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.interpolate_value_keeping(x, keep)).collect()),
            Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), self.interpolate_value_keeping(x, keep))).collect()),
            other => other.clone(),
        }
    }

    /// [`Self::interpolate`], keeping `${{ }}` whose first name is in `keep` as written.
    pub fn interpolate_keeping(&self, text: &str, keep: &[&str]) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${{") {
            out.push_str(rest.get(..start).unwrap_or(""));
            let after = rest.get(start + 3..).unwrap_or("");
            let Some(end) = after.find("}}") else {
                out.push_str(rest.get(start..).unwrap_or(""));
                return out;
            };
            let expr = after.get(..end).unwrap_or("");
            let root = expr.trim().split('.').next().unwrap_or("");
            if keep.contains(&root) {
                out.push_str("${{");
                out.push_str(expr);
                out.push_str("}}");
                rest = after.get(end + 2..).unwrap_or("");
                continue;
            }
            match self.lookup(expr) {
                Some(Value::String(s)) => out.push_str(s),
                Some(Value::Null) | None => {}
                Some(other) => out.push_str(&other.to_string()),
            }
            rest = after.get(end + 2..).unwrap_or("");
        }
        out.push_str(rest);
        out
    }
}

/// Reads `.kiln/deploy.yaml` from `repo` into app definitions: one per app and environment.
/// `environments` is trunk's `.kiln/environments.yaml`; `trunk` is the branch apps follow when
/// they don't name a revision; `scope` holds `git.*` and the like.
pub fn from_deploy_file(
    text: &str,
    repo: &str,
    trunk: &str,
    environments: &BTreeMap<String, EnvironmentSpec>,
    scope: &Scope,
) -> Result<Vec<AppDef>, DefError> {
    let file: DeployFile = serde_yaml::from_str(text).map_err(|e| DefError(format!(".kiln/deploy.yaml: {e}")))?;
    let mut out = Vec::new();
    for (name, raw) in &file.apps {
        let (spec, provider) = split_provider(name, raw)?;
        let provider_name = provider.as_ref().map(|(p, _)| p.as_str());
        // Each destination: (environment, cluster or target, namespace, the target's settings).
        let places: Vec<Place> = match &spec.to {
            None if provider.is_some() => return Err(DefError(format!("app {name} deploys with {}: say where with to: (an environment, or a target)", provider_name.unwrap_or_default()))),
            None => vec![(None, IN_CLUSTER.to_owned(), None, Map::new())],
            Some(ToSpec::Place { cluster, namespace }) => {
                if let Some(p) = provider_name
                    && crate::providers::Target::parse(cluster).is_none_or(|t| t.provider != p)
                {
                    return Err(DefError(format!("app {name} deploys with {p}, so to: names a {p} target (`{p}/<account>/<place>`), not `{cluster}`")));
                }
                vec![(None, cluster.clone(), namespace.clone(), Map::new())]
            }
            Some(ToSpec::One(env)) => env_places(env, environments, name, provider_name)?,
            Some(ToSpec::Many(envs)) => {
                let mut all = Vec::new();
                for env in envs {
                    all.extend(env_places(env, environments, name, provider_name)?);
                }
                all
            }
        };
        for (env, cluster, ns, target) in places {
            let mut scope = scope.clone();
            scope.set("app.name", Value::String(name.clone()));
            scope.set("cluster.name", Value::String(cluster.clone()));
            if let Some(t) = crate::providers::Target::parse(&cluster) {
                scope.set("target.name", Value::String(cluster.clone()));
                scope.set("target.account", Value::String(t.account.clone()));
                for (k, v) in t.def().map(|d| d.place).unwrap_or_default().iter().zip(&t.place) {
                    scope.set(&format!("target.{k}"), Value::String(v.clone()));
                }
                for (k, v) in &target {
                    scope.set(&format!("target.{k}"), v.clone());
                }
            }
            if let Some(e) = &env {
                scope.set("env.name", Value::String(e.clone()));
            }
            // web, web-staging, or web-production-prod-us when an environment has several clusters.
            let app_name = match &env {
                None => name.clone(),
                Some(e) if environments.get(e).is_some_and(|x| x.clusters.len() > 1 && provider.is_none()) => format!("{name}-{e}-{cluster}"),
                Some(e) => format!("{name}-{e}"),
            };
            let p = provider.as_ref().map(|(p, v)| (p.as_str(), v, &target));
            out.push(app_from_spec(&app_name, &spec, p, &file.checks, repo, trunk, env, &cluster, ns, &scope)?);
        }
    }
    Ok(out)
}

/// Where an app deploys: its environment, its cluster (or target), the namespace it names, and
/// its target's settings beyond account and place.
type Place = (Option<String>, String, Option<String>, Map<String, Value>);

fn env_places(env: &str, environments: &BTreeMap<String, EnvironmentSpec>, app: &str, provider: Option<&str>) -> Result<Vec<Place>, DefError> {
    let e = environments
        .get(env)
        .ok_or_else(|| DefError(format!("app {app} deploys to environment {env}, which .kiln/environments.yaml doesn't define")))?;
    if let Some(p) = provider {
        let spec = e.targets.get(p).ok_or_else(|| DefError(format!("app {app} deploys with {p} to environment {env}, which has no {p} target: add `targets: {{ {p}: {{ account: <integration>, … }} }}` to it")))?;
        let (target, extra) = crate::providers::target_of(p, spec).map_err(|m| DefError(format!("environment {env}: {m}")))?;
        return Ok(vec![(Some(env.to_owned()), target.name(), None, extra)]);
    }
    if e.clusters.is_empty() {
        return Err(DefError(format!("environment {env} has no clusters")));
    }
    Ok(e.clusters.iter().map(|c| (Some(env.to_owned()), c.clone(), None, Map::new())).collect())
}

#[allow(clippy::too_many_arguments)]
fn app_from_spec(
    name: &str,
    spec: &AppSpec,
    provider: Option<(&str, &Value, &Map<String, Value>)>,
    file_checks: &BTreeMap<String, Value>,
    repo: &str,
    trunk: &str,
    environment: Option<String>,
    cluster: &str,
    namespace: Option<String>,
    scope: &Scope,
) -> Result<AppDef, DefError> {
    let i = |t: &str| scope.interpolate(t);
    let mut specs: Vec<&SourceSpec> = spec.sources.iter().collect();
    if let Some(src) = &spec.source {
        specs.insert(0, src);
    }
    // A provider's app reads its files from its source, by default the repository's root.
    let root = SourceSpec::Path(String::new());
    if specs.is_empty() && provider.is_some() {
        specs.push(&root);
    }
    if specs.is_empty() {
        return Err(DefError(format!("app {name} has no source")));
    }
    if let Some((p, _, _)) = provider
        && (spec.helm.is_some() || spec.kustomize.is_some() || spec.directory.is_some() || spec.plugin.is_some())
    {
        return Err(DefError(format!("app {name} deploys with {p}: helm, kustomize, directory and plugin are for Kubernetes apps")));
    }
    let mut sources = Vec::new();
    for src in specs {
        sources.push(match src {
            SourceSpec::Path(p) => Source { repo: repo.to_owned(), path: i(p), revision: trunk.to_owned(), ..Source::default() },
            SourceSpec::Full { repo: r, path, r#ref, chart, version, as_ref } => Source {
                repo: r.as_deref().map(i).unwrap_or_else(|| repo.to_owned()),
                path: path.as_deref().map(i).unwrap_or_default(),
                revision: version.as_deref().or(r#ref.as_deref()).map(i).unwrap_or_else(|| trunk.to_owned()),
                chart: chart.clone(),
                as_ref: as_ref.clone(),
                engine: Engine::Auto,
            },
        });
    }
    // Engine settings apply to the first source that renders (not a `ref`-only one).
    if let Some(first) = sources.iter_mut().find(|s| s.as_ref.is_none() || s.chart.is_some()) {
        if let Some((p, v, target)) = provider {
            // `git.*` stays for the revision each render is at, `artifacts.*` for what it resolved.
            first.engine = Engine::Provider(ProviderSource { provider: p.to_owned(), spec: scope.interpolate_value_keeping(v, &["git", "artifacts"]), target: target.clone() });
        } else if let Some(h) = &spec.helm {
            let mut parameters: Vec<HelmParam> = h
                .set
                .iter()
                .map(|(k, v)| HelmParam {
                    name: k.clone(),
                    value: match v {
                        Value::String(s) => i(s),
                        other => other.to_string(),
                    },
                    force_string: false,
                })
                .collect();
            parameters.extend(h.set_string.iter().map(|(k, v)| HelmParam { name: k.clone(), value: i(v), force_string: true }));
            first.engine = Engine::Helm(Helm {
                value_files: h.values.iter().map(|v| i(v)).collect(),
                values: h.values_object.clone(),
                parameters,
                file_parameters: h.set_file.iter().map(|(k, v)| (k.clone(), i(v))).collect(),
                release_name: h.release_name.clone(),
                skip_crds: h.skip_crds,
                skip_tests: h.skip_tests,
                ..Helm::default()
            });
        } else if let Some(k) = &spec.kustomize {
            first.engine = Engine::Kustomize(Kustomize {
                images: strings(k, "images"),
                name_prefix: s(k, "name-prefix"),
                name_suffix: s(k, "name-suffix"),
                common_labels: string_map(k, "labels"),
                common_annotations: string_map(k, "annotations"),
                namespace: s(k, "namespace"),
                replicas: k
                    .get("replicas")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(n, c)| (n.clone(), obj::as_i64(c).unwrap_or(1))).collect())
                    .unwrap_or_default(),
                patches: k.get("patches").and_then(Value::as_array).cloned().unwrap_or_default(),
                components: strings(k, "components"),
            });
        } else if let Some(d) = &spec.directory {
            first.engine = Engine::Directory(d.clone());
        } else if let Some(p) = &spec.plugin {
            first.engine = Engine::Plugin(Plugin {
                name: s(p, "name").unwrap_or_default(),
                env: string_map(p, "env").into_iter().collect(),
                parameters: p.get("parameters").and_then(Value::as_array).cloned().unwrap_or_default(),
            });
        }
    }
    let sync = match &spec.sync {
        Some(sy) => {
            let (options, _) = SyncOptions::parse(sy.options.iter().map(String::as_str));
            SyncPolicy {
                auto: sy.auto,
                prune: sy.prune,
                self_heal: sy.self_heal,
                allow_empty: sy.allow_empty,
                options,
                retry: sy.retry.as_ref().map(|r| Retry {
                    limit: r.limit,
                    backoff_ms: r.backoff.as_deref().and_then(duration_ms).unwrap_or(5_000),
                    factor: r.factor.unwrap_or(2),
                    max_ms: r.max.as_deref().and_then(duration_ms).unwrap_or(180_000),
                }),
            }
        }
        None => SyncPolicy::default(),
    };
    // Checks: the file's, then the app's own; `rollout.*` and `result.*` wait for the rollout.
    let mut checks = BTreeMap::new();
    for (n, raw) in file_checks.iter().chain(spec.checks.iter()) {
        let v = scope.interpolate_value_keeping(raw, &["git", "artifacts", "rollout", "result"]);
        let c: crate::rollout::CheckSpec = serde_json::from_value(v).map_err(|e| DefError(format!("check {n}: {e}")))?;
        checks.insert(n.clone(), c);
    }
    if let Some(r) = &spec.rollout {
        crate::rollout::compile(r, &checks).map_err(|e| DefError(format!("app {name}: rollout: {e}")))?;
    }
    for (n, a) in &spec.artifacts {
        let kinds = [!a.image.is_empty(), a.published.is_some(), a.artifact.is_some()].iter().filter(|x| **x).count();
        if kinds != 1 {
            return Err(DefError(format!("app {name}: artifact {n} is one of: an image, a published image, or a workflow's artifact (workflow and artifact)")));
        }
        if a.artifact.is_some() != a.workflow.is_some() {
            return Err(DefError(format!("app {name}: artifact {n} needs both the workflow and its artifact")));
        }
    }
    Ok(AppDef {
        name: name.to_owned(),
        project: spec.project.clone().unwrap_or_else(|| "default".into()),
        sources,
        destination: Destination {
            cluster: cluster.to_owned(),
            namespace: match provider {
                // A provider's scope (an ECS cluster) is its definition's, if it has one.
                Some((p, v, _)) => crate::providers::get(p).and_then(|d| d.scope).and_then(|k| v.get(k)).and_then(Value::as_str).map(i).unwrap_or_default(),
                None => namespace.or_else(|| spec.namespace.as_deref().map(i)).unwrap_or_else(|| "default".into()),
            },
        },
        sync,
        ignore: spec
            .ignore
            .iter()
            .map(|r| IgnoreRule {
                group: r.group.clone(),
                kind: r.kind.clone(),
                name: r.name.clone(),
                namespace: r.namespace.clone(),
                json_pointers: r.pointers.clone(),
                jq_paths: r.jq.clone(),
                managers: r.managers.clone(),
            })
            .collect(),
        tracking: Tracking::Kiln,
        tracking_name: None,
        origin: Origin::Deploy { repo: repo.to_owned() },
        environment,
        labels: spec.labels.clone(),
        history_limit: 10,
        cascade: true,
        managed_namespace: None,
        // `git.*` stays for the revision each resolves at (a rollback's, say); the rest (`env`,
        // `app`) is known now.
        artifacts: spec
            .artifacts
            .iter()
            .map(|(n, a)| {
                let k = |t: &str| scope.interpolate_keeping(t, &["git", "artifacts"]);
                (
                    n.clone(),
                    ArtifactSpec {
                        image: k(&a.image),
                        published: a.published.as_deref().map(k),
                        pipeline: a.pipeline.clone(),
                        workflow: a.workflow.as_deref().map(k),
                        artifact: a.artifact.as_deref().map(k),
                        tag: a.tag.as_deref().map(k),
                        registry: a.registry.as_deref().map(k),
                        pull_secret: a.pull_secret.as_deref().map(k),
                        platforms: a.platforms.clone(),
                    },
                )
            })
            .collect(),
        rollout: spec.rollout.clone(),
        checks,
        secrets: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_app_taking_argo_cd_s_objects_over_knows_them_by_its_application_s_name() {
        let mut d = crate::state::tests::app("podinfo-staging");
        assert_eq!(d.config().tracking_name, None, "kiln's own tracking: the app's id");
        d.tracking = Tracking::ArgoAnnotation;
        assert_eq!(d.config().tracking_name.as_deref(), Some("podinfo-staging"), "imported: the Application is the app");
        d.tracking_name = Some("staging-podinfo".into());
        let c = d.config();
        assert_eq!(c.tracking_name.as_deref(), Some("staging-podinfo"), "taken over: the Application it was");
        assert_eq!(&*c.manager, "argocd-controller", "what Argo CD owned stays owned");
    }

    #[test]
    fn go_durations_parse() {
        assert_eq!(duration_ms("5s"), Some(5_000));
        assert_eq!(duration_ms("3m"), Some(180_000));
        assert_eq!(duration_ms("1h30m"), Some(5_400_000));
        assert_eq!(duration_ms("250ms"), Some(250));
        assert_eq!(duration_ms("10"), Some(10_000));
        assert_eq!(duration_ms("x"), None);
    }

    #[test]
    fn argo_applications_keep_their_meaning() -> Result<(), DefError> {
        // kiln's own kube-prometheus-stack app (deploy/argocd/apps/templates/applications.yaml).
        let app = json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "Application",
            "metadata": {"name": "kube-prometheus-stack", "namespace": "argocd", "finalizers": ["resources-finalizer.argocd.argoproj.io"]},
            "spec": {"project": "default",
                "sources": [
                    {"repoURL": "https://prometheus-community.github.io/helm-charts", "chart": "kube-prometheus-stack", "targetRevision": "92.2.0",
                     "helm": {"releaseName": "kube-prometheus-stack", "valueFiles": ["$values/deploy/argocd/values/kube-prometheus-stack.yaml"]}},
                    {"repoURL": "http://kiln-git.kiln-git.svc.cluster.local/kiln.git", "targetRevision": "main", "ref": "values"}],
                "destination": {"server": "https://kubernetes.default.svc", "namespace": "monitoring"},
                "syncPolicy": {"automated": {"prune": true, "selfHeal": true}, "syncOptions": ["CreateNamespace=true", "ServerSideApply=true"],
                    "retry": {"limit": 10, "backoff": {"duration": "10s", "factor": 2, "maxDuration": "3m"}}}}});
        let d = from_argo(&app, Some("kiln-apps"), &|_| None)?;
        assert_eq!(d.id(), "default/kube-prometheus-stack");
        assert_eq!(d.destination, Destination { cluster: IN_CLUSTER.into(), namespace: "monitoring".into() });
        assert!(d.sync.auto && d.sync.prune && d.sync.self_heal && d.cascade);
        assert_eq!(d.sync.options.create_namespace, Some(true));
        assert_eq!(d.sync.retry, Some(Retry { limit: 10, backoff_ms: 10_000, factor: 2, max_ms: 180_000 }));
        assert_eq!(d.sources.len(), 2);
        assert_eq!(d.sources.get(1).and_then(|s| s.as_ref.clone()), Some("values".into()));
        match d.sources.first().map(|s| &s.engine) {
            Some(Engine::Helm(h)) => assert_eq!(h.value_files, ["$values/deploy/argocd/values/kube-prometheus-stack.yaml"]),
            other => return Err(DefError(format!("expected helm, got {other:?}"))),
        }
        Ok(())
    }

    #[test]
    fn deploy_files_expand_environments() -> Result<(), DefError> {
        let envs: BTreeMap<String, EnvironmentSpec> = serde_yaml::from_str(
            "staging: { clusters: [dev] }\nproduction: { clusters: [prod-us, prod-eu], after: staging }\n",
        )
        .map_err(|e| DefError(e.to_string()))?;
        let mut scope = Scope::default();
        scope.set("git.short_sha", json!("abc1234"));
        let text = r#"
apps:
  web:
    source: deploy/web
    helm:
      values: [values.yaml, "values-${{ env.name }}.yaml"]
      set: { image.tag: "${{ git.short_sha }}", replicas: 3 }
    to: [staging, production]
    namespace: web
    sync: { auto: true, prune: true, self-heal: true, options: [CreateNamespace=true] }
"#;
        let apps = from_deploy_file(text, "hyper-light/site", "main", &envs, &scope)?;
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["web-staging", "web-production-prod-us", "web-production-prod-eu"]);
        let prod = apps.get(1).ok_or_else(|| DefError("missing".into()))?;
        assert_eq!(prod.destination, Destination { cluster: "prod-us".into(), namespace: "web".into() });
        match prod.sources.first().map(|s| &s.engine) {
            Some(Engine::Helm(h)) => {
                assert_eq!(h.value_files, ["values.yaml", "values-production.yaml"]);
                assert!(h.parameters.contains(&HelmParam { name: "image.tag".into(), value: "abc1234".into(), force_string: false }));
                assert!(h.parameters.contains(&HelmParam { name: "replicas".into(), value: "3".into(), force_string: false }));
            }
            other => return Err(DefError(format!("expected helm, got {other:?}"))),
        }
        assert!(from_deploy_file("apps: { x: { source: a, to: nowhere } }", "r", "main", &envs, &scope).is_err());
        Ok(())
    }

    /// docs/cd-targets.md §4: an app names its provider's key; each environment its target.
    #[test]
    fn provider_apps_deploy_to_their_environments_targets() -> Result<(), DefError> {
        let envs: BTreeMap<String, EnvironmentSpec> = serde_yaml::from_str(
            "staging: { targets: { aws-ecs: { account: aws-staging, region: us-east-1 } } }\nproduction: { clusters: [prod], targets: { aws-ecs: { account: aws-prod, region: us-west-2, suffix: -prod } } }\n",
        )
        .map_err(|e| DefError(e.to_string()))?;
        let text = r#"
apps:
  api:
    aws-ecs:
      cluster: "main${{ target.suffix }}"
      service: api
      task: deploy/api/task.json
      set: { "containerDefinitions[name=api].image": "${{ artifacts.image }}", "service.desiredCount": 2 }
    artifacts:
      image: { image: "docker.io/acme/api", tag: "${{ git.sha }}" }
    to: [staging, production]
"#;
        let apps = from_deploy_file(text, "acme/api", "main", &envs, &Scope::default())?;
        let names: Vec<(&str, &str, &str)> = apps.iter().map(|a| (a.name.as_str(), a.destination.cluster.as_str(), a.destination.namespace.as_str())).collect();
        assert_eq!(names, [("api-staging", "aws-ecs/aws-staging/us-east-1", "main"), ("api-production", "aws-ecs/aws-prod/us-west-2", "main-prod")]);
        let prod = apps.get(1).ok_or_else(|| DefError("no production app".into()))?;
        let source = prod.sources.first().ok_or_else(|| DefError("no source".into()))?;
        assert_eq!(source.path, "", "the repository's root by default");
        let Engine::Provider(p) = &source.engine else { return Err(DefError(format!("expected a provider, got {:?}", source.engine))) };
        assert_eq!(p.provider, "aws-ecs");
        assert_eq!(p.spec.pointer("/set/containerDefinitions[name=api].image"), Some(&json!("${{ artifacts.image }}")), "artifacts wait for the revision");
        assert_eq!(p.target.get("suffix"), Some(&json!("-prod")));

        // Rendered from the source, with the revision's artifacts.
        let dir = std::env::temp_dir().join(format!("kiln-provider-render-{}", std::process::id()));
        let task = dir.join("deploy/api");
        std::fs::create_dir_all(&task).map_err(|e| DefError(e.to_string()))?;
        std::fs::write(task.join("task.json"), r#"{"family":"api","containerDefinitions":[{"name":"api","image":"x"}]}"#).map_err(|e| DefError(e.to_string()))?;
        let digest = format!("sha256:{}", "d".repeat(64));
        let refs = [ArtifactRef { name: "image".into(), reference: format!("docker.io/acme/api@{digest}"), repository: "docker.io/acme/api".into(), tag: None, digest }];
        let sources = with_artifacts(&prod.sources, &refs)?;
        let source = sources.first().ok_or_else(|| DefError("no source".into()))?;
        let (refs_dirs, scratch) = (BTreeMap::new(), dir.join("scratch"));
        let req = crate::render::Request { source, root: &dir, refs: &refs_dirs, app: &prod.name, namespace: &prod.destination.namespace, revision: "abc1234def", kube_version: None, api_versions: &[], scratch: &scratch };
        let objs = crate::render::render(&req, &crate::render::Tools::default()).map_err(|e| DefError(format!("{e:#}")))?;
        let _ = std::fs::remove_dir_all(&dir);
        let td = objs.first().ok_or_else(|| DefError("no task definition".into()))?;
        assert_eq!(td.pointer("/spec/containerDefinitions/0/image").and_then(Value::as_str), Some(refs.first().map(|r| r.reference.as_str()).unwrap_or_default()));
        let svc = objs.get(1).ok_or_else(|| DefError("no service".into()))?;
        assert_eq!((crate::obj::namespace(svc), svc.pointer("/spec/desiredCount")), ("main-prod", Some(&json!(2))));
        let deps = crate::render::deps(source, crate::render::Kind::Provider, &|_| None);
        assert_eq!(deps.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(), ["deploy/api/task.json"]);

        // A provider's app says where it goes, to a target of its own provider.
        assert!(from_deploy_file("apps: { x: { aws-ecs: { cluster: c, service: s, task: t.json } } }", "r", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("to:")));
        assert!(from_deploy_file("apps: { x: { aws-lambda: { function: f, code: s3://b/k }, to: production } }", "r", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("no aws-lambda target")));
        assert!(from_deploy_file("apps: { x: { aws-ecs: {}, aws-lambda: {}, to: production } }", "r", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("one provider")));
        assert!(from_deploy_file("apps: { x: { aws-ecs: {}, to: { cluster: in-cluster } } }", "r", "main", &envs, &Scope::default()).is_err());
        Ok(())
    }

    /// docs/cd-targets.md §5: a rollout and the checks it uses read from the deploy file, the
    /// rollout's context waiting for the rollout; the traffic-carrying object carries them.
    #[test]
    fn rollouts_and_checks_read_and_stamp() -> Result<(), DefError> {
        let envs: BTreeMap<String, EnvironmentSpec> = serde_yaml::from_str("production: { targets: { aws-lambda: { account: aws, region: us-east-1 } } }").map_err(|e| DefError(e.to_string()))?;
        let text = r#"
checks:
  smoke:
    http: { url: "${{ rollout.preview_url }}/healthz" }
    pass: "${{ result.status == 200 }}"
apps:
  api:
    aws-lambda: { function: api, code: s3://b/k.zip, set: { Runtime: python3.13, Handler: a.h, Role: r } }
    rollout: { blue-green: { before: [smoke, e2e], keep: 1h } }
    checks:
      e2e: { script: { run: "./e2e.sh ${{ env.name }}" } }
    to: production
"#;
        let apps = from_deploy_file(text, "acme/api", "main", &envs, &Scope::default())?;
        let app = apps.first().ok_or_else(|| DefError("no app".into()))?;
        assert!(app.rollout.as_ref().is_some_and(|r| r.blue_green.is_some()));
        assert_eq!(app.checks.get("smoke").and_then(|c| c.http.clone()), Some(json!({ "url": "${{ rollout.preview_url }}/healthz" })));
        assert_eq!(app.checks.get("e2e").and_then(|c| c.script.clone()), Some(json!({ "run": "./e2e.sh production" })), "known now: env.name");
        let mut objs = vec![json!({ "apiVersion": "lambda.aws.kiln.dev/v1", "kind": "Alias", "metadata": { "name": "api:live" } }), json!({ "apiVersion": "lambda.aws.kiln.dev/v1", "kind": "Function", "metadata": { "name": "api" } })];
        stamp_rollout(app, &mut objs);
        assert!(objs.first().and_then(|o| obj::annotation(o, ROLLOUT)).is_some_and(|r| r.contains("blue_green") || r.contains("blue-green")));
        assert!(objs.get(1).and_then(|o| obj::annotation(o, ROLLOUT)).is_none(), "only what carries traffic");
        // A rollout naming a check nobody defined is a deploy file error.
        let bad = text.replace("before: [smoke, e2e]", "before: [nope]");
        assert!(from_deploy_file(&bad, "acme/api", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("no check named nope")));
        Ok(())
    }

    /// kiln's own deploy files (its production reads them every round) read as they always did.
    #[test]
    fn kilns_own_deploy_files_read() -> Result<(), DefError> {
        let envs: BTreeMap<String, EnvironmentSpec> = serde_yaml::from_str(include_str!("../../../.kiln/environments.yaml")).map_err(|e| DefError(e.to_string()))?;
        let mut scope = Scope::default();
        scope.set("git.sha", json!("1111111111111111111111111111111111111111"));
        let apps = from_deploy_file(include_str!("../../../.kiln/deploy.yaml"), "https://github.com/adalundhe/kiln.git", "main", &envs, &scope)?;
        let mut names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["kiln-production", "kube-prometheus-stack-production", "namespaces-production"]);
        assert!(apps.iter().all(|a| a.destination.cluster == IN_CLUSTER && !matches!(a.sources.first().map(|s| &s.engine), Some(Engine::Provider(_)))));
        Ok(())
    }

    /// docs/cd-targets.md §7: a file a workflow uploaded is an artifact too; an artifact is one
    /// kind of thing.
    #[test]
    fn workflow_artifacts_read_and_artifacts_are_one_thing() -> Result<(), DefError> {
        let envs: BTreeMap<String, EnvironmentSpec> = serde_yaml::from_str("production: { targets: { aws-lambda: { account: aws, region: us-east-1 } } }").map_err(|e| DefError(e.to_string()))?;
        let text = r#"
apps:
  thumbs:
    aws-lambda: { function: thumbs, code: "${{ artifacts.zip }}", set: { Runtime: python3.13, Handler: app.handler, Role: r } }
    artifacts:
      zip: { workflow: ci, artifact: thumbs.zip }
    to: production
"#;
        let apps = from_deploy_file(text, "acme/thumbs", "main", &envs, &Scope::default())?;
        let a = apps.first().and_then(|a| a.artifacts.get("zip")).ok_or_else(|| DefError("no artifact".into()))?;
        assert_eq!((a.workflow.as_deref(), a.artifact.as_deref()), (Some("ci"), Some("thumbs.zip")));
        let both = "apps: { x: { aws-lambda: { function: f, code: c }, artifacts: { zip: { image: a/b, workflow: ci, artifact: z } }, to: production } }";
        assert!(from_deploy_file(both, "r", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("one of")));
        let half = "apps: { x: { aws-lambda: { function: f, code: c }, artifacts: { zip: { artifact: z } }, to: production } }";
        assert!(from_deploy_file(half, "r", "main", &envs, &Scope::default()).is_err_and(|e| e.0.contains("both")));
        Ok(())
    }

    #[test]
    fn artifacts_resolve_per_revision_and_fill_the_sources() -> Result<(), DefError> {
        let text = r#"
apps:
  kiln:
    source: deploy/helm/kiln
    helm:
      set: { image.repository: "${{ artifacts.image.repository }}", image.digest: "${{ artifacts.image.digest }}", image.ref: "${{ artifacts.image }}" }
    artifacts:
      image: { image: "docker.io/acme/kiln", tag: "${{ git.sha }}", registry: dockerhub, pull-secret: "kiln-${{ app.name }}" }
    namespace: kiln
"#;
        let mut scope = Scope::default();
        scope.set("git.sha", json!("1111111111111111111111111111111111111111"));
        let apps = from_deploy_file(text, "hyper-light/kiln", "main", &BTreeMap::new(), &scope)?;
        let app = apps.first().ok_or_else(|| DefError("no app".into()))?;
        let a = app.artifacts.get("image").ok_or_else(|| DefError("no artifact".into()))?;
        // `git.sha` waits for the revision; `app.name` is known when the file is read.
        assert_eq!(a.tag.as_deref(), Some("${{ git.sha }}"));
        assert_eq!(a.pull_secret.as_deref(), Some("kiln-kiln"));
        assert_eq!(a.reference("2222222222222222222222222222222222222222"), "docker.io/acme/kiln:2222222222222222222222222222222222222222");
        // The helm values keep `artifacts.*` until the revision resolves it.
        let set = serde_json::to_string(&app.sources).unwrap_or_default();
        assert!(set.contains("${{ artifacts.image.digest }}"), "{set}");
        let digest = format!("sha256:{}", "c".repeat(64));
        let refs = [ArtifactRef { name: "image".into(), reference: format!("docker.io/acme/kiln@{digest}"), repository: "docker.io/acme/kiln".into(), tag: Some("2222222".into()), digest: digest.clone() }];
        let filled = serde_json::to_string(&with_artifacts(&app.sources, &refs)?).unwrap_or_default();
        assert!(filled.contains(&format!("docker.io/acme/kiln@{digest}")) && filled.contains("\"image.repository\"") && !filled.contains("${{"), "{filled}");
        // A name no artifact has is an error, not an empty image.
        assert!(substitute_artifacts("${{ artifacts.imgae }}", &refs).is_err_and(|e| e.0.contains("no artifact `imgae`")));
        assert!(substitute_artifacts("${{ artifacts.image.size }}", &refs).is_err());
        // Other expressions pass through.
        assert_eq!(substitute_artifacts("a ${{ env.name }} b", &refs)?, "a ${{ env.name }} b");
        Ok(())
    }

    #[test]
    fn an_apps_secrets_are_one_secret_and_the_pods_that_read_them_roll_with_their_versions() {
        let s = app_secret("web-kiln", "shop", "A@1,B@2", [("A", &b"one"[..]), ("B", &b"two"[..])]);
        assert_eq!(obj::kind(&s), "Secret");
        assert_eq!(obj::str_at(&s, &["data", "A"]), Some("b25l"));
        assert_eq!(obj::annotation(&s, SECRETS), Some("A@1,B@2"));
        let reads = |spec: Value| json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"}, "spec": {"template": {"spec": spec}}});
        let mut objects = vec![
            reads(json!({"containers": [{"name": "app", "envFrom": [{"secretRef": {"name": "web-kiln", "optional": true}}]}]})),
            reads(json!({"containers": [{"name": "app", "env": [{"name": "A", "valueFrom": {"secretKeyRef": {"name": "web-kiln", "key": "A"}}}]}]})),
            reads(json!({"containers": [{"name": "app"}], "volumes": [{"name": "s", "projected": {"sources": [{"secret": {"name": "web-kiln"}}]}}]})),
            reads(json!({"containers": [{"name": "app", "envFrom": [{"secretRef": {"name": "other"}}]}]})),
            json!({"apiVersion": "batch/v1", "kind": "CronJob", "metadata": {"name": "nightly"}, "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "c"}], "volumes": [{"name": "s", "secret": {"secretName": "web-kiln"}}]}}}}}}),
        ];
        stamp_secrets(&mut objects, "web-kiln", "shop", "A@1,B@2");
        let stamped: Vec<bool> = objects
            .iter()
            .map(|o| {
                let at: &[&str] = if obj::kind(o) == "CronJob" { &["spec", "jobTemplate", "spec", "template", "metadata", "annotations", SECRETS] } else { &["spec", "template", "metadata", "annotations", SECRETS] };
                obj::str_at(o, at) == Some("A@1,B@2")
            })
            .collect();
        assert_eq!(stamped, [true, true, true, false, true]);
    }

    #[test]
    fn a_pull_secret_holds_the_docker_config() {
        let s = pull_secret("kiln-registry", "kiln", r#"{"auths":{}}"#);
        assert_eq!(obj::kind(&s), "Secret");
        assert_eq!(s.pointer("/type").and_then(Value::as_str), Some("kubernetes.io/dockerconfigjson"));
        assert_eq!(s.pointer("/data/.dockerconfigjson").and_then(Value::as_str), Some("eyJhdXRocyI6e319"));
        assert_eq!(s.pointer("/metadata/annotations/argocd.argoproj.io~1sync-wave").and_then(Value::as_str), Some("-10"));
    }
}
