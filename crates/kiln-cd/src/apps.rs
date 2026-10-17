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
        origin: Origin::Argo { parent: parent.map(str::to_owned) },
        environment: None,
        labels: obj::map_at(v, &["metadata", "labels"])
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
            .unwrap_or_default(),
        history_limit: spec.get("revisionHistoryLimit").and_then(obj::as_i64).map(|n| n.clamp(0, 1000) as u32).unwrap_or(10),
        cascade,
        managed_namespace,
    })
}

/// kiln's deploy file (`.kiln/deploy.yaml`), as written.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployFile {
    /// Apps by name.
    #[serde(default)]
    pub apps: BTreeMap<String, AppSpec>,
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
    /// The environment it follows.
    #[serde(default)]
    pub after: Option<String>,
    /// How long a revision must run healthy in `after` first.
    #[serde(default)]
    pub soak: Option<String>,
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

    /// Replaces every `${{ path }}` in `text`; unknown names are empty, as in pipelines.
    pub fn interpolate(&self, text: &str) -> String {
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
    for (name, spec) in &file.apps {
        // Each destination: (environment, cluster, namespace).
        let places: Vec<(Option<String>, String, Option<String>)> = match &spec.to {
            None => vec![(None, IN_CLUSTER.to_owned(), None)],
            Some(ToSpec::Place { cluster, namespace }) => vec![(None, cluster.clone(), namespace.clone())],
            Some(ToSpec::One(env)) => env_places(env, environments, name)?,
            Some(ToSpec::Many(envs)) => {
                let mut all = Vec::new();
                for env in envs {
                    all.extend(env_places(env, environments, name)?);
                }
                all
            }
        };
        for (env, cluster, ns) in places {
            let mut scope = scope.clone();
            scope.set("app.name", Value::String(name.clone()));
            scope.set("cluster.name", Value::String(cluster.clone()));
            if let Some(e) = &env {
                scope.set("env.name", Value::String(e.clone()));
            }
            // web, web-staging, or web-production-prod-us when an environment has several clusters.
            let app_name = match &env {
                None => name.clone(),
                Some(e) if environments.get(e).is_some_and(|x| x.clusters.len() > 1) => format!("{name}-{e}-{cluster}"),
                Some(e) => format!("{name}-{e}"),
            };
            out.push(app_from_spec(&app_name, spec, repo, trunk, env, &cluster, ns, &scope)?);
        }
    }
    Ok(out)
}

fn env_places(
    env: &str,
    environments: &BTreeMap<String, EnvironmentSpec>,
    app: &str,
) -> Result<Vec<(Option<String>, String, Option<String>)>, DefError> {
    let e = environments
        .get(env)
        .ok_or_else(|| DefError(format!("app {app} deploys to environment {env}, which .kiln/environments.yaml doesn't define")))?;
    if e.clusters.is_empty() {
        return Err(DefError(format!("environment {env} has no clusters")));
    }
    Ok(e.clusters.iter().map(|c| (Some(env.to_owned()), c.clone(), None)).collect())
}

#[allow(clippy::too_many_arguments)]
fn app_from_spec(
    name: &str,
    spec: &AppSpec,
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
    if specs.is_empty() {
        return Err(DefError(format!("app {name} has no source")));
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
        if let Some(h) = &spec.helm {
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
    Ok(AppDef {
        name: name.to_owned(),
        project: spec.project.clone().unwrap_or_else(|| "default".into()),
        sources,
        destination: Destination {
            cluster: cluster.to_owned(),
            namespace: namespace.or_else(|| spec.namespace.as_deref().map(i)).unwrap_or_else(|| "default".into()),
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
        origin: Origin::Deploy { repo: repo.to_owned() },
        environment,
        labels: spec.labels.clone(),
        history_limit: 10,
        cascade: true,
        managed_namespace: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
