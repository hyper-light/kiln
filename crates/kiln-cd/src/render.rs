//! Rendering a source into objects: plain directories, Helm charts, Kustomize builds and
//! config management plugins, with Argo CD's rules for each.
//!
//! A render reads a materialized view of the repository ([`Request::root`]) holding exactly
//! the paths [`deps`] said it reads, so the files it can see are the files its key covers.
//! Helm and Kustomize run as their own binaries for now (kiln's native engines will take over,
//! checked against these); everything runs with a timeout, on the server's blocking pool.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};

use crate::apps::{Engine, Helm, Kustomize, Plugin, Source};
use crate::obj;

/// The programs renders run, and how.
#[derive(Clone, Debug)]
pub struct Tools {
    /// `helm`.
    pub helm: PathBuf,
    /// `kustomize`; `kubectl kustomize` when unset.
    pub kustomize: Option<PathBuf>,
    /// `kubectl`.
    pub kubectl: PathBuf,
    /// Extra `kustomize build` options (Argo CD's `kustomize.buildOptions`).
    pub kustomize_options: Vec<String>,
    /// Plugins by name.
    pub plugins: BTreeMap<String, PluginCommand>,
    /// Longest a render may take.
    pub timeout: Duration,
    /// Helm's cache, configuration and data home (kept apart from the user's).
    pub helm_home: Option<PathBuf>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            helm: "helm".into(),
            kustomize: None,
            kubectl: "kubectl".into(),
            kustomize_options: Vec::new(),
            plugins: BTreeMap::new(),
            timeout: Duration::from_secs(90),
            helm_home: None,
        }
    }
}

/// A plugin's commands (Argo CD's `plugin.yaml` `init` and `generate`).
#[derive(Clone, Debug, Default)]
pub struct PluginCommand {
    /// Run first, output ignored.
    pub init: Vec<String>,
    /// Prints the manifests.
    pub generate: Vec<String>,
}

/// What a source renders with, as detected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A Helm chart.
    Helm,
    /// A Kustomize build.
    Kustomize,
    /// Plain manifests.
    Directory,
    /// A plugin.
    Plugin,
}

const KUSTOMIZATIONS: [&str; 3] = ["kustomization.yaml", "kustomization.yml", "Kustomization"];

/// The engine a source uses: what it says, or what its files say (Argo CD's detection order:
/// a chart, then a kustomization, then a directory).
pub fn kind_of(source: &Source, has_file: &dyn Fn(&str) -> bool) -> Kind {
    match &source.engine {
        Engine::Helm(_) => Kind::Helm,
        Engine::Kustomize(_) => Kind::Kustomize,
        Engine::Directory(_) => Kind::Directory,
        Engine::Plugin(_) => Kind::Plugin,
        Engine::Auto if source.chart.is_some() => Kind::Helm,
        Engine::Auto => {
            let at = |f: &str| join(&source.path, f).is_some_and(|p| has_file(&p));
            if at("Chart.yaml") {
                Kind::Helm
            } else if KUSTOMIZATIONS.iter().any(|k| at(k)) {
                Kind::Kustomize
            } else {
                Kind::Directory
            }
        }
    }
}

/// `rel` joined to `base` inside the repository, with `.` and `..` resolved. `None` when it
/// would leave the repository (Argo CD refuses those too).
pub fn join(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let joined = if rel.starts_with('/') { rel.to_owned() } else { format!("{base}/{rel}") };
    for c in Path::new(&joined).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Some(parts.join("/"))
}

/// A path a render reads: in the source's own repository, or in a `ref` source's.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Dep {
    /// The `ref` source the path is in, or `None` for the source's own repository.
    pub source: Option<String>,
    /// The path, relative to that repository's root.
    pub path: String,
}

/// What a source reads, found from its files before rendering: the key of a render covers the
/// hashes of these paths. `read` gives a file's text in the source's repository at the revision.
pub fn deps(source: &Source, kind: Kind, read: &dyn Fn(&str) -> Option<String>) -> Vec<Dep> {
    let own = |path: String| Dep { source: None, path };
    let mut out = Vec::new();
    if source.chart.is_some() {
        // A chart from a Helm repository: only `ref` value files come from git.
        if let Engine::Helm(h) = &source.engine {
            out.extend(helm_value_deps(source, h));
        }
        return out;
    }
    match kind {
        Kind::Directory | Kind::Plugin => out.push(own(source.path.clone())),
        Kind::Helm => {
            out.push(own(source.path.clone()));
            if let Engine::Helm(h) = &source.engine {
                out.extend(helm_value_deps(source, h));
            }
        }
        Kind::Kustomize => {
            let mut seen = Vec::new();
            kustomize_deps(&source.path, read, &mut seen, 0);
            out.extend(seen.into_iter().map(own));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn helm_value_deps(source: &Source, h: &Helm) -> Vec<Dep> {
    let mut out = Vec::new();
    for f in h.value_files.iter().chain(h.file_parameters.iter().map(|(_, p)| p)) {
        if f.contains("://") {
            continue;
        }
        if let Some(rest) = f.strip_prefix('$') {
            if let Some((name, path)) = rest.split_once('/') {
                out.push(Dep { source: Some(name.to_owned()), path: path.to_owned() });
            }
        } else if let Some(p) = join(&source.path, f) {
            out.push(Dep { source: None, path: p });
        }
    }
    out
}

/// Every local path a kustomization reads, following bases and components.
fn kustomize_deps(dir: &str, read: &dyn Fn(&str) -> Option<String>, seen: &mut Vec<String>, depth: u32) {
    if depth > 32 || seen.iter().any(|s| s == dir) {
        return;
    }
    seen.push(dir.to_owned());
    let Some(text) = KUSTOMIZATIONS.iter().find_map(|k| join(dir, k).and_then(|p| read(&p))) else { return };
    let Ok(k) = serde_yaml::from_str::<Value>(&text) else { return };
    let mut paths: Vec<String> = Vec::new();
    let mut take = |v: Option<&Value>| {
        for item in v.and_then(Value::as_array).into_iter().flatten() {
            if let Some(s) = item.as_str() {
                paths.push(s.to_owned());
            } else if let Some(p) = item.get("path").and_then(Value::as_str) {
                paths.push(p.to_owned());
            }
        }
    };
    for key in ["resources", "bases", "components", "patchesStrategicMerge", "crds", "configurations", "generators", "transformers", "validators"] {
        take(k.get(key));
    }
    take(k.get("patches"));
    take(k.get("patchesJson6902"));
    take(k.get("replacements"));
    for generator in ["configMapGenerator", "secretGenerator"] {
        for g in k.get(generator).and_then(Value::as_array).into_iter().flatten() {
            for key in ["files", "envs"] {
                for f in g.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                    paths.push(f.rsplit_once('=').map(|(_, p)| p).unwrap_or(f).to_owned());
                }
            }
            if let Some(e) = g.get("env").and_then(Value::as_str) {
                paths.push(e.to_owned());
            }
        }
    }
    if let Some(p) = k.get("openapi").and_then(|o| o.get("path")).and_then(Value::as_str) {
        paths.push(p.to_owned());
    }
    if k.get("helmCharts").is_some() {
        let home = k.get("helmGlobals").and_then(|g| g.get("chartHome")).and_then(Value::as_str).unwrap_or("charts");
        paths.push(home.to_owned());
        for c in k.get("helmCharts").and_then(Value::as_array).into_iter().flatten() {
            if let Some(v) = c.get("valuesFile").and_then(Value::as_str) {
                paths.push(v.to_owned());
            }
        }
    }
    for p in paths {
        // Inline patches and remote resources are not paths in the repository.
        if p.contains('\n') || p.contains("://") || p.starts_with("github.com/") || p.contains("?ref=") {
            continue;
        }
        let Some(full) = join(dir, &p) else { continue };
        if KUSTOMIZATIONS.iter().any(|k| join(&full, k).is_some_and(|kp| read(&kp).is_some())) {
            kustomize_deps(&full, read, seen, depth + 1);
        } else if !seen.contains(&full) {
            seen.push(full);
        }
    }
}

/// One render.
pub struct Request<'a> {
    /// The source.
    pub source: &'a Source,
    /// The materialized repository view (repository-relative layout).
    pub root: &'a Path,
    /// `ref` sources' materialized views, by name.
    pub refs: &'a BTreeMap<String, PathBuf>,
    /// The app's name (the default Helm release name; `ARGOCD_APP_NAME`).
    pub app: &'a str,
    /// The destination namespace.
    pub namespace: &'a str,
    /// The commit (`ARGOCD_APP_REVISION`).
    pub revision: &'a str,
    /// The destination's Kubernetes version (`1.35.1`).
    pub kube_version: Option<&'a str>,
    /// The destination's API versions (`apps/v1`, `monitoring.coreos.com/v1/ServiceMonitor`).
    pub api_versions: &'a [String],
    /// Where temporary files may go.
    pub scratch: &'a Path,
}

impl Request<'_> {
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("ARGOCD_APP_NAME".into(), self.app.into()),
            ("ARGOCD_APP_NAMESPACE".into(), self.namespace.into()),
            ("ARGOCD_APP_REVISION".into(), self.revision.into()),
            ("ARGOCD_APP_REVISION_SHORT".into(), self.revision.get(..7).unwrap_or(self.revision).into()),
            ("ARGOCD_APP_SOURCE_REPO_URL".into(), self.source.repo.clone()),
            ("ARGOCD_APP_SOURCE_PATH".into(), self.source.path.clone()),
            ("ARGOCD_APP_SOURCE_TARGET_REVISION".into(), self.source.revision.clone()),
            ("KUBE_VERSION".into(), self.kube_version.unwrap_or("").into()),
            ("KUBE_API_VERSIONS".into(), self.api_versions.join(",")),
            ("KILN_APP".into(), self.app.into()),
            ("KILN_SHA".into(), self.revision.into()),
        ]
    }

    /// Argo CD's build-environment substitution in parameters: `$ARGOCD_APP_NAME`,
    /// `${ARGOCD_APP_NAMESPACE}` and the rest.
    fn substitute(&self, text: &str) -> String {
        let mut out = text.to_owned();
        let mut env = self.env();
        env.sort_by_key(|e| std::cmp::Reverse(e.0.len()));
        for (k, v) in env {
            out = out.replace(&format!("${{{k}}}"), &v).replace(&format!("${k}"), &v);
        }
        out
    }

    fn dir(&self) -> PathBuf {
        self.root.join(&self.source.path)
    }
}

/// Renders a source.
pub fn render(req: &Request<'_>, tools: &Tools) -> Result<Vec<Value>> {
    let has = |p: &str| req.root.join(p).is_file();
    match kind_of(req.source, &has) {
        Kind::Directory => {
            let d = match &req.source.engine {
                Engine::Directory(d) => d.clone(),
                _ => Default::default(),
            };
            directory(&req.dir(), d.recurse, d.include.as_deref(), d.exclude.as_deref())
        }
        Kind::Helm => {
            let h = match &req.source.engine {
                Engine::Helm(h) => h.clone(),
                _ => Helm::default(),
            };
            helm(req, &h, tools)
        }
        Kind::Kustomize => {
            let k = match &req.source.engine {
                Engine::Kustomize(k) => k.clone(),
                _ => Kustomize::default(),
            };
            kustomize(req, &k, tools)
        }
        Kind::Plugin => {
            let p = match &req.source.engine {
                Engine::Plugin(p) => p.clone(),
                _ => Plugin::default(),
            };
            plugin(req, &p, tools)
        }
    }
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (pattern.find('{'), pattern.find('}')) else { return vec![pattern.to_owned()] };
    if close < open {
        return vec![pattern.to_owned()];
    }
    let (head, rest) = (pattern.get(..open).unwrap_or(""), pattern.get(close + 1..).unwrap_or(""));
    pattern
        .get(open + 1..close)
        .unwrap_or("")
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{rest}")))
        .collect()
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    expand_braces(pattern).iter().any(|p| glob::Pattern::new(p).is_ok_and(|g| g.matches(path)))
}

/// A directory's YAML and JSON documents.
pub fn directory(dir: &Path, recurse: bool, include: Option<&str>, exclude: Option<&str>) -> Result<Vec<Value>> {
    let mut files = Vec::new();
    walk(dir, dir, recurse, &mut files)?;
    files.sort();
    let mut out = Vec::new();
    for rel in files {
        let ext_ok = rel.ends_with(".yaml") || rel.ends_with(".yml") || rel.ends_with(".json");
        if rel.ends_with(".jsonnet") || rel.ends_with(".libsonnet") {
            let wanted = include.is_none_or(|p| glob_matches(p, &rel)) && !exclude.is_some_and(|p| glob_matches(p, &rel));
            if wanted {
                bail!("{rel}: Jsonnet sources are not supported yet");
            }
            continue;
        }
        if !ext_ok || include.is_some_and(|p| !glob_matches(p, &rel)) || exclude.is_some_and(|p| glob_matches(p, &rel)) {
            continue;
        }
        let text = std::fs::read_to_string(dir.join(&rel)).with_context(|| format!("reading {rel}"))?;
        let docs = obj::parse_documents(&text).with_context(|| format!("parsing {rel}"))?;
        out.extend(docs.into_iter().filter(Value::is_object));
    }
    Ok(out)
}

fn walk(base: &Path, dir: &Path, recurse: bool, out: &mut Vec<String>) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let ft = e.file_type()?;
        let path = e.path();
        if ft.is_dir() {
            if recurse {
                walk(base, &path, recurse, out)?;
            }
        } else if ft.is_file()
            && let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
    }
    Ok(())
}

/// Runs a program in `dir` with `env`, within `timeout`, returning its standard output.
fn exec(mut c: Command, dir: &Path, env: &[(String, String)], timeout: Duration) -> Result<String> {
    c.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    let program = format!("{:?}", c.get_program());
    let mut child = c.spawn().with_context(|| format!("running {program}"))?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    // Read both pipes on threads so neither fills and blocks the child.
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).map(|_| s)
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).map(|_| s)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            child.kill().ok();
            child.wait().ok();
            bail!("{program} took longer than {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out_t.join().map_err(|_| anyhow::anyhow!("reading {program}'s output"))??;
    let err = err_t.join().map_err(|_| anyhow::anyhow!("reading {program}'s errors"))??;
    if !status.success() {
        bail!("{program} failed: {}", err.trim());
    }
    Ok(out)
}

fn helm_env(tools: &Tools, base: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut env = base;
    if let Some(home) = &tools.helm_home {
        env.push(("HELM_CACHE_HOME".into(), home.join("cache").to_string_lossy().into_owned()));
        env.push(("HELM_CONFIG_HOME".into(), home.join("config").to_string_lossy().into_owned()));
        env.push(("HELM_DATA_HOME".into(), home.join("data").to_string_lossy().into_owned()));
    }
    env
}

fn helm(req: &Request<'_>, h: &Helm, tools: &Tools) -> Result<Vec<Value>> {
    let env = helm_env(tools, req.env());
    let release = h.release_name.clone().unwrap_or_else(|| req.app.to_owned());
    let mut c = Command::new(&tools.helm);
    c.arg("template").arg(&release);
    let chart_dir = req.dir();
    match (&req.source.chart, req.source.repo.as_str()) {
        (Some(chart), repo) if repo.starts_with("oci://") => {
            c.arg(format!("{}/{chart}", repo.trim_end_matches('/'))).arg("--version").arg(&req.source.revision);
        }
        (Some(chart), repo) => {
            c.arg(chart).arg("--repo").arg(repo).arg("--version").arg(&req.source.revision);
        }
        (None, _) => {
            build_dependencies(&chart_dir, tools, &env)?;
            c.arg(".");
        }
    }
    c.arg("--namespace").arg(h.namespace.as_deref().unwrap_or(req.namespace));
    for f in &h.value_files {
        let path = if f.contains("://") {
            PathBuf::from(f)
        } else if let Some(rest) = f.strip_prefix('$') {
            let (name, path) = rest.split_once('/').unwrap_or((rest, ""));
            let root = req.refs.get(name).with_context(|| format!("value file {f}: no source is named {name}"))?;
            root.join(path)
        } else {
            let p = join(&req.source.path, f).with_context(|| format!("value file {f} is outside the repository"))?;
            req.root.join(p)
        };
        if !f.contains("://") && !path.exists() {
            if h.ignore_missing_value_files {
                continue;
            }
            bail!("value file {f} doesn't exist");
        }
        c.arg("--values").arg(path);
    }
    if let Some(values) = &h.values {
        std::fs::create_dir_all(req.scratch)?;
        let file = req.scratch.join(format!("values-{}.yaml", obj::hash_value(values)));
        std::fs::write(&file, serde_yaml::to_string(values)?)?;
        c.arg("--values").arg(file);
    }
    for p in &h.parameters {
        let flag = if p.force_string { "--set-string" } else { "--set" };
        c.arg(flag).arg(format!("{}={}", p.name, req.substitute(&p.value)));
    }
    for (name, path) in &h.file_parameters {
        let p = join(&req.source.path, path).with_context(|| format!("file parameter {path} is outside the repository"))?;
        c.arg("--set-file").arg(format!("{name}={}", req.root.join(p).display()));
    }
    if let Some(v) = h.kube_version.as_deref().or(req.kube_version) {
        c.arg("--kube-version").arg(v);
    }
    let api_versions = if h.api_versions.is_empty() { req.api_versions } else { &h.api_versions };
    for v in api_versions {
        c.arg("--api-versions").arg(v);
    }
    c.arg(if h.skip_crds { "--skip-crds" } else { "--include-crds" });
    if h.skip_tests {
        c.arg("--skip-tests");
    }
    if h.skip_schema_validation {
        c.arg("--skip-schema-validation");
    }
    let dir = if req.source.chart.is_some() { req.scratch.to_path_buf() } else { chart_dir };
    std::fs::create_dir_all(&dir)?;
    let out = exec(c, &dir, &env, tools.timeout)?;
    obj::parse_documents(&out).context("parsing helm's output")
}

/// `helm dependency build` when the chart declares dependencies its `charts/` lacks.
fn build_dependencies(chart: &Path, tools: &Tools, env: &[(String, String)]) -> Result<()> {
    let Ok(text) = std::fs::read_to_string(chart.join("Chart.yaml")) else { return Ok(()) };
    let meta: Value = serde_yaml::from_str(&text).unwrap_or(Value::Null);
    let deps = meta.get("dependencies").and_then(Value::as_array).cloned().unwrap_or_default();
    if deps.is_empty() {
        return Ok(());
    }
    let have = |name: &str| {
        let charts = chart.join("charts");
        charts.join(name).is_dir()
            || std::fs::read_dir(&charts)
                .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("{name}-"))))
                .unwrap_or(false)
    };
    if deps.iter().all(|d| d.get("name").and_then(Value::as_str).is_some_and(have)) {
        return Ok(());
    }
    // Repositories by URL, added under names of their own so `dependency build` finds them.
    for d in &deps {
        let Some(url) = d.get("repository").and_then(Value::as_str) else { continue };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let mut c = Command::new(&tools.helm);
        c.args(["repo", "add", "--force-update", &format!("kiln-{:016x}", obj::hash_bytes(url.as_bytes())), url]);
        exec(c, chart, env, tools.timeout)?;
    }
    let mut c = Command::new(&tools.helm);
    c.args(["dependency", "build", "--skip-refresh", "."]);
    exec(c, chart, env, tools.timeout).map(|_| ())
}

fn kustomize(req: &Request<'_>, k: &Kustomize, tools: &Tools) -> Result<Vec<Value>> {
    let dir = req.dir();
    if has_overrides(k) {
        apply_overrides(&dir, k, req)?;
    }
    let mut c = match &tools.kustomize {
        Some(bin) => {
            let mut c = Command::new(bin);
            c.arg("build");
            c
        }
        None => {
            let mut c = Command::new(&tools.kubectl);
            c.arg("kustomize");
            c
        }
    };
    c.arg(".");
    for o in &tools.kustomize_options {
        c.arg(o);
    }
    let out = exec(c, &dir, &req.env(), tools.timeout)?;
    obj::parse_documents(&out).context("parsing kustomize's output")
}

fn has_overrides(k: &Kustomize) -> bool {
    !k.images.is_empty()
        || k.name_prefix.is_some()
        || k.name_suffix.is_some()
        || !k.common_labels.is_empty()
        || !k.common_annotations.is_empty()
        || k.namespace.is_some()
        || !k.replicas.is_empty()
        || !k.patches.is_empty()
        || !k.components.is_empty()
}

/// Argo CD's Kustomize overrides, written into the (private, materialized) kustomization as
/// `kustomize edit` would.
fn apply_overrides(dir: &Path, k: &Kustomize, req: &Request<'_>) -> Result<()> {
    let file = KUSTOMIZATIONS.iter().map(|f| dir.join(f)).find(|p| p.is_file()).context("no kustomization to override")?;
    let mut doc: Value = serde_yaml::from_str(&std::fs::read_to_string(&file)?)?;
    let m = doc.as_object_mut().context("the kustomization is not a mapping")?;
    if !k.images.is_empty() {
        let images = m.entry("images").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(list) = images.as_array_mut() {
            for spec in &k.images {
                let spec = req.substitute(spec);
                let (name, rest) = match spec.split_once('=') {
                    Some((n, r)) => (n.to_owned(), Some(r.to_owned())),
                    None => (spec.split([':', '@']).next().unwrap_or("").to_owned(), None),
                };
                let mut entry = Map::new();
                entry.insert("name".into(), Value::String(name.clone()));
                let target = rest.unwrap_or_else(|| spec.clone());
                let (img, digest) = match target.split_once('@') {
                    Some((i, d)) => (i.to_owned(), Some(d.to_owned())),
                    None => (target, None),
                };
                let (new_name, tag) = match img.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                    Some((n, t)) => (n.to_owned(), Some(t.to_owned())),
                    None => (img, None),
                };
                if new_name != name {
                    entry.insert("newName".into(), Value::String(new_name));
                }
                if let Some(t) = tag {
                    entry.insert("newTag".into(), Value::String(t));
                }
                if let Some(d) = digest {
                    entry.insert("digest".into(), Value::String(d));
                }
                list.retain(|e| e.get("name").and_then(Value::as_str) != Some(name.as_str()));
                list.push(Value::Object(entry));
            }
        }
    }
    if let Some(p) = &k.name_prefix {
        m.insert("namePrefix".into(), Value::String(req.substitute(p)));
    }
    if let Some(s) = &k.name_suffix {
        m.insert("nameSuffix".into(), Value::String(req.substitute(s)));
    }
    for (key, values) in [("commonLabels", &k.common_labels), ("commonAnnotations", &k.common_annotations)] {
        if values.is_empty() {
            continue;
        }
        let slot = m.entry(key).or_insert_with(|| Value::Object(Map::new()));
        if let Some(obj) = slot.as_object_mut() {
            for (lk, lv) in values {
                obj.insert(lk.clone(), Value::String(req.substitute(lv)));
            }
        }
    }
    if let Some(ns) = &k.namespace {
        m.insert("namespace".into(), Value::String(req.substitute(ns)));
    }
    if !k.replicas.is_empty() {
        let slot = m.entry("replicas").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(list) = slot.as_array_mut() {
            for (name, count) in &k.replicas {
                list.retain(|e| e.get("name").and_then(Value::as_str) != Some(name.as_str()));
                list.push(serde_json::json!({"name": name, "count": count}));
            }
        }
    }
    if !k.patches.is_empty() {
        let slot = m.entry("patches").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(list) = slot.as_array_mut() {
            list.extend(k.patches.iter().cloned());
        }
    }
    if !k.components.is_empty() {
        let slot = m.entry("components").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(list) = slot.as_array_mut() {
            list.extend(k.components.iter().map(|c| Value::String(c.clone())));
        }
    }
    std::fs::write(&file, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn plugin(req: &Request<'_>, p: &Plugin, tools: &Tools) -> Result<Vec<Value>> {
    let cmd = tools.plugins.get(&p.name).with_context(|| format!("no plugin is named {:?}", p.name))?;
    let mut env = req.env();
    for (k, v) in &p.env {
        env.push((format!("ARGOCD_ENV_{k}"), req.substitute(v)));
    }
    env.push(("ARGOCD_APP_PARAMETERS".into(), Value::Array(p.parameters.clone()).to_string()));
    let dir = req.dir();
    let run = |argv: &[String]| -> Result<String> {
        let (prog, args) = argv.split_first().context("a plugin command is empty")?;
        let mut c = Command::new(prog);
        c.args(args);
        exec(c, &dir, &env, tools.timeout)
    };
    if !cmd.init.is_empty() {
        run(&cmd.init)?;
    }
    let out = run(&cmd.generate)?;
    obj::parse_documents(&out).context("parsing the plugin's output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::Directory;

    #[test]
    fn joins_stay_inside_the_repository() {
        assert_eq!(join("deploy/helm/kiln", "../../argocd/values/kiln.yaml"), Some("deploy/argocd/values/kiln.yaml".into()));
        assert_eq!(join("deploy", "./a/./b"), Some("deploy/a/b".into()));
        assert_eq!(join("deploy", "../../etc/passwd"), None);
        assert_eq!(join("", "x"), Some("x".into()));
    }

    #[test]
    fn directories_honor_recurse_include_and_exclude() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let d = tmp.path();
        std::fs::create_dir_all(d.join("sub"))?;
        std::fs::write(d.join("a.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: a}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata: {name: b}\n")?;
        std::fs::write(d.join("c.json"), r#"{"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "c"}}"#)?;
        std::fs::write(d.join("README.md"), "# not a manifest")?;
        std::fs::write(d.join("sub/d.yml"), "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: d}\n")?;
        let names = |v: Vec<Value>| v.iter().map(|o| obj::name(o).to_owned()).collect::<Vec<_>>();
        assert_eq!(names(directory(d, false, None, None)?), ["a", "b", "c"]);
        assert_eq!(names(directory(d, true, None, None)?), ["a", "b", "c", "d"]);
        assert_eq!(names(directory(d, true, Some("{a.yaml,sub/*}"), None)?), ["a", "b", "d"]);
        assert_eq!(names(directory(d, true, None, Some("*.json"))?), ["a", "b", "d"]);
        Ok(())
    }

    #[test]
    fn kustomize_deps_follow_bases_and_files() {
        let files: BTreeMap<&str, &str> = [
            ("apps/web/overlays/prod/kustomization.yaml", "resources: [../../base, extra.yaml]\npatches: [{path: patch.yaml}]\nconfigMapGenerator: [{name: c, files: [cfg=conf/app.conf]}]\n"),
            ("apps/web/base/kustomization.yaml", "resources: [deploy.yaml, https://example.com/x.yaml]\ncomponents: [../../../components/monitoring]\n"),
            ("components/monitoring/kustomization.yaml", "kind: Component\nresources: [sm.yaml]\n"),
        ]
        .into_iter()
        .collect();
        let read = |p: &str| files.get(p).map(|s| (*s).to_owned());
        let src = Source { path: "apps/web/overlays/prod".into(), ..Source::default() };
        let d: Vec<String> = deps(&src, Kind::Kustomize, &read).into_iter().map(|d| d.path).collect();
        assert_eq!(
            d,
            [
                "apps/web/base",
                "apps/web/base/deploy.yaml",
                "apps/web/overlays/prod",
                "apps/web/overlays/prod/conf/app.conf",
                "apps/web/overlays/prod/extra.yaml",
                "apps/web/overlays/prod/patch.yaml",
                "components/monitoring",
                "components/monitoring/sm.yaml",
            ]
        );
    }

    #[test]
    fn helm_deps_include_value_files_outside_the_chart_and_refs() {
        let src = Source {
            path: "deploy/helm/kiln".into(),
            engine: Engine::Helm(Helm { value_files: vec!["values.yaml".into(), "../../argocd/values/kiln.yaml".into(), "$values/prod.yaml".into()], ..Helm::default() }),
            ..Source::default()
        };
        let d = deps(&src, Kind::Helm, &|_| None);
        assert_eq!(
            d,
            [
                Dep { source: None, path: "deploy/argocd/values/kiln.yaml".into() },
                Dep { source: None, path: "deploy/helm/kiln".into() },
                Dep { source: None, path: "deploy/helm/kiln/values.yaml".into() },
                Dep { source: Some("values".into()), path: "prod.yaml".into() },
            ]
        );
    }

    #[test]
    fn detection_prefers_charts_then_kustomizations() {
        let src = Source { path: "x".into(), ..Source::default() };
        assert_eq!(kind_of(&src, &|p| p == "x/Chart.yaml"), Kind::Helm);
        assert_eq!(kind_of(&src, &|p| p == "x/kustomization.yml"), Kind::Kustomize);
        assert_eq!(kind_of(&src, &|_| false), Kind::Directory);
        let d = Source { engine: Engine::Directory(Directory::default()), ..src };
        assert_eq!(kind_of(&d, &|_| true), Kind::Directory);
    }
}

#[cfg(test)]
mod real_tools {
    //! Renders kiln's own deploy tree with the real `helm`, through a git mirror, when helm is
    //! installed (it is skipped where it isn't).
    use super::*;
    use crate::git::{Auth, Mirror};

    fn have(bin: &str) -> bool {
        Command::new(bin).arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    #[test]
    fn kilns_own_chart_renders_from_its_mirror() -> Result<()> {
        if !have("helm") {
            return Ok(());
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tmp = tempfile::tempdir()?;
        let m = Mirror::open(&tmp.path().join("kiln.git"), repo.to_str().unwrap_or("."), Auth::default())?;
        let commit = m.resolve("HEAD")?;
        let src = Source {
            repo: "kiln".into(),
            path: "deploy/helm/kiln".into(),
            revision: "HEAD".into(),
            engine: Engine::Helm(Helm {
                value_files: vec!["values.yaml".into(), "../../argocd/values/kiln.yaml".into()],
                release_name: Some("kiln".into()),
                ..Helm::default()
            }),
            ..Source::default()
        };
        let read = |p: &str| m.read(&commit, p).ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
        let kind = kind_of(&src, &|p| read(p).is_some());
        assert_eq!(kind, Kind::Helm);
        let d: Vec<String> = deps(&src, kind, &read).into_iter().map(|d| d.path).collect();
        // The chart links dashboards and rules to deploy/grafana and deploy/prometheus.
        let d = m.with_links(&commit, &d)?;
        assert!(d.iter().any(|p| p == "deploy/grafana/dashboards") && d.iter().any(|p| p == "deploy/prometheus"), "{d:?}");
        let paths: Vec<&str> = d.iter().map(String::as_str).collect();
        let view = tmp.path().join("view");
        m.extract(&commit, &paths, &view)?;
        let refs = BTreeMap::new();
        let req = Request {
            source: &src,
            root: &view,
            refs: &refs,
            app: "kiln",
            namespace: "kiln",
            revision: &commit,
            kube_version: Some("1.35.1"),
            api_versions: &[],
            scratch: &tmp.path().join("scratch"),
        };
        let objects = render(&req, &Tools::default())?;
        let kinds: Vec<&str> = objects.iter().map(obj::kind).collect();
        assert!(kinds.contains(&"Deployment"), "{kinds:?}");
        assert!(kinds.contains(&"Service"), "{kinds:?}");
        Ok(())
    }
}
