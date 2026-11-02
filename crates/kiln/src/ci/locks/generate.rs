//! W29-R25: bounded source collection for one atomic draft. This reads source facts and
//! accepted files; only the existing draft writer may publish the completed pair of locks.

use kiln_ci::lock::{dockerfile, kiln, regenerate, verify_kiln};
use kiln_seams::dependencies::{Manifest, Refusal, Runtime};

use super::*;

#[derive(Clone)]
struct Tree { owner: Owner, edits: Arc<BTreeMap<String, Option<Vec<u8>>>> }

#[derive(Clone)]
struct Context {
    workflow: String,
    home: String,
    github: Option<Arc<github::File>>,
    layers: Vec<native::Layer>,
}

struct Collector<'a> {
    hub: &'a Hub,
    caller: &'a Repo,
    source: &'a dyn DependencySources,
    root: Tree,
    root_workflow: String,
    root_name: String,
    github: Option<Arc<github::File>>,
    native: Option<Arc<kiln::File>>,
    revisions: BTreeMap<String, Revision>,
    resolutions: BTreeMap<String, kiln::Dependency>,
    texts: BTreeMap<String, Option<Arc<str>>>,
    rules: Vec<kiln_ci::sched::routing::Rule>,
    pools: Vec<kiln_ci::sched::pools::Pool>,
    image_locks: kiln_ci::lock::execution::Locks,
    manifests: BTreeMap<String, Manifest>,
    active: BTreeSet<String>,
    bytes: usize,
    items: usize,
    mode: regenerate::Mode,
}

fn diagnostics(path: &str, ds: Vec<wf::diag::Diag>) -> String { ds.iter().map(|d| d.render(path)).collect::<Vec<_>>().join("\n") }

impl Collector<'_> {
    fn count(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.items = self.items.saturating_add(1);
        if self.bytes > MAX_BYTES || self.items > MAX_ITEMS { return Err("KCI1022 lock generation exceeds 16 MiB or 4096 inputs".into()); }
        Ok(())
    }

    async fn file(&mut self, tree: &Tree, path: &str) -> Result<Option<String>, String> {
        if !kiln_seams::dependencies::path_valid(path) { return Err(format!("KCI1022 {path}: dependency path must stay in its repository")); }
        let key = format!("{:?}:{}:{}:{path}", tree.owner.revision.as_ref().map(|r| &r.identity), tree.owner.repository, tree.owner.commit);
        if let Some(text) = self.texts.get(&key) { return Ok(text.as_deref().map(str::to_owned)); }
        let bytes = if let Some(bytes) = tree.edits.get(path) { bytes.clone() } else { tree.owner.file(self.hub, self.source, path).await? };
        let text = bytes.map(|bytes| {
            self.count(bytes.len())?;
            if bytes.len() > kiln_seams::dependencies::FILE_MAX { return Err(format!("KCI1022 {path}: dependency file exceeds 1 MiB")); }
            String::from_utf8(bytes).map_err(|_| format!("KCI1022 {path}: dependency file is not UTF-8"))
        }).transpose()?;
        self.texts.insert(key, text.as_deref().map(Arc::from));
        Ok(text)
    }

    async fn context(&mut self, tree: &Tree, path: &str, inherited: &[native::Layer]) -> Result<Context, String> {
        let home = tree.owner.home_host(self.hub, self.caller);
        let github = if path.starts_with(".github/workflows/") {
            self.file(tree, github::PATH).await?.map(|s| github::parse(&s).map(Arc::new).map_err(|ds| diagnostics(github::PATH, ds))).transpose()?
        } else { None };
        let file = self.file(tree, kiln::PATH).await?.map(|s| kiln::parse(&s).map(Arc::new).map_err(|ds| diagnostics(kiln::PATH, ds))).transpose()?;
        if let (Some(github), Some(file)) = (&github, &file) { regenerate::compare(github, file, &home).map_err(|ds| diagnostics(path, ds))?; }
        let mut layers = inherited.to_vec();
        if let Some(file) = file.filter(|f| f.workflows.contains_key(path)) {
            layers.push(native::Layer { repository: tree.owner.repository.clone(), workflow: path.into(), home: home.clone(), file });
        }
        Ok(Context { workflow: path.into(), home, github, layers })
    }

    fn add(&mut self, key: &str, dependency: kiln::Dependency) -> Result<(), String> {
        let key = kiln::canonical_key(key).ok_or_else(|| format!("KCI4050 {key}: a dependency needs a literal reference"))?;
        if let Some(old) = self.resolutions.get_mut(&key) {
            match (old, &dependency) {
                (kiln::Dependency::Git(a), kiln::Dependency::Git(b)) if a.git_ref == b.git_ref && a.commit == b.commit && a.repo == b.repo && a.hostname == b.hostname => {
                    a.uses = a.uses.iter().chain(&b.uses).cloned().collect::<BTreeSet<_>>().into_iter().collect();
                }
                (a, b) if a == b => {}
                (a, b) => return Err(format!("KCI4054 {key}: the draft reaches conflicting source facts {a:?} and {b:?}")),
            }
        } else { self.count(0)?; self.resolutions.insert(key, dependency); }
        Ok(())
    }

    fn accepted(&self, context: &Context, uses: &str) -> Result<Option<kiln::Dependency>, String> {
        let home = self.root.owner.home_host(self.hub, self.caller);
        if let Some(pin) = regenerate::accepted(self.github.as_deref(), self.native.as_deref(), &self.root_workflow, uses, &home).map_err(|ds| diagnostics(&context.workflow, ds))? { return Ok(Some(pin)); }
        for layer in &context.layers {
            if let Some((_, git)) = verify_kiln::pin_for(&layer.file, &layer.workflow, uses) {
                let mut git = git.clone();
                if git.hostname.is_none() && matches!(git.identity(&layer.home), Some(kiln::Identity::Github { .. })) { git.hostname = Some(layer.home.clone()); }
                return Ok(Some(kiln::Dependency::Git(git)));
            }
            let key = kiln::canonical_key(uses).ok_or_else(|| format!("KCI4050 {uses}: invalid dependency key"))?;
            if layer.file.closure(&layer.workflow).is_some_and(|keys| keys.contains(&key))
                && let Some(pin) = layer.file.dependencies.get(&key) { return Ok(Some(pin.clone())); }
        }
        regenerate::accepted(context.github.as_deref(), None, &context.workflow, uses, &context.home).map_err(|ds| diagnostics(&context.workflow, ds))
    }

    async fn resolve(&mut self, remote: &Remote, reference: &str) -> Result<Revision, String> {
        let key = Capture::ref_key(remote, reference);
        if let Some(revision) = self.revisions.get(&key) { return Ok(revision.clone()); }
        self.count(0)?;
        remote.usable_by(self.hub, &self.caller.name).await?;
        let revision = self.source.resolve(Repository { name: remote.full(), native: remote.native }, reference.into()).await.map_err(|e| format!("KCI4051 {}: {e}", remote.named()))?;
        revision.validate().map_err(|e| format!("KCI4051 {}: {e}", remote.named()))?;
        self.revisions.insert(key, revision.clone());
        Ok(revision)
    }

    async fn git(&mut self, uses: &str, context: &Context) -> Result<(kiln::Git, Tree, Remote), String> {
        if let Some(why) = kiln_ci::drafts::uses_of_another(uses, &self.caller.name) { return Err(format!("KCI4023 {why}")); }
        let remote = Remote::parse(uses).ok_or_else(|| format!("KCI4050 {uses}: dependency needs a literal source"))?;
        let mut accepted = match self.accepted(context, uses)? {
            Some(kiln::Dependency::Git(g)) => Some(g),
            Some(_) => return Err(format!("KCI4054 {uses}: its accepted pin is not a Git source")),
            None => None,
        };
        let reference = accepted.as_ref().map_or(remote.git_ref.as_str(), |g| g.git_ref.as_str()).to_owned();
        let immutable = kiln_seams::dependencies::commit_valid(&reference);
        let revision = self.resolve(&remote, if immutable { "HEAD" } else { &reference }).await?;
        let seen = history::Seen::read(self.hub, self.source, &remote, &revision).await?;
        if accepted.is_none() && self.mode == regenerate::Mode::Preserve { accepted = seen.accepted_pin()?; }
        let identity = match &revision.identity {
            Identity::Github { hostname, owner_id, repo_id } => kiln::Identity::Github { hostname: hostname.clone(), owner_id: *owner_id, repo_id: *repo_id },
            Identity::Native { repo_id } => kiln::Identity::Kiln { id: repo_id.clone() },
        };
        let (repo, hostname) = match &identity {
            kiln::Identity::Github { hostname, owner_id, repo_id } => (format!("github:{owner_id}/{repo_id}"), Some(hostname.clone())),
            kiln::Identity::Kiln { id } => (format!("kiln:{id}"), None),
        };
        let wanted = if immutable { reference.to_ascii_lowercase() } else { revision.commit.clone() };
        let current = kiln::Git { git_ref: revision.reference.clone(), commit: format!("{}-{wanted}", if wanted.len() == 64 { "sha256" } else { "sha1" }), repo, hostname, uses: Vec::new() };
        let mut reachable = BTreeSet::new();
        for sha in accepted.as_ref().and_then(|g| github::digest(&g.commit)).into_iter().chain(std::iter::once(wanted.as_str())) {
            if self.source.contains(revision.clone(), sha.into()).await.map_err(|e| format!("KCI4051 {uses}: {e}"))? { reachable.insert(sha.to_owned()); }
        }
        let pin = regenerate::source(regenerate::SourceCheck {
            workflow: &context.workflow, key: &kiln::canonical_key(uses).ok_or_else(|| format!("KCI4050 {uses}: invalid source key"))?,
            accepted: accepted.as_ref(), current: &current, facts: &verify_kiln::GitFacts { identity, reference: revision.reference.clone(), current: revision.commit.clone(), reachable, previous: seen.previous.clone() },
            home_host: &context.home, mode: self.mode,
        }).map_err(|ds| diagnostics(&context.workflow, ds))?;
        let commit = github::digest(&pin.commit).ok_or_else(|| format!("KCI1022 {uses}: source commit is invalid"))?.to_owned();
        let tree = Tree { owner: Owner { repository: remote.full(), commit, remote: Some(remote.clone()), revision: Some(revision) }, edits: Arc::new(BTreeMap::new()) };
        Ok((pin, tree, remote))
    }

    async fn image(&mut self, key: &str, context: &Context) -> Result<String, String> {
        let key = kiln::canonical_key(key).ok_or_else(|| format!("KCI4050 {key}: image needs a literal reference"))?;
        let reference = ["docker://", "image://", "executor://", "microvm://"].iter().find_map(|p| key.strip_prefix(p)).ok_or_else(|| format!("KCI4050 {key}: image reference is invalid"))?;
        let mut accepted = kiln_ci::lock::execution::Locks::default();
        if self.mode == regenerate::Mode::Preserve {
            let exact = self.accepted(context, &key)?;
            for prefix in ["docker://", "image://", "executor://", "microvm://"] {
                let alias = format!("{prefix}{reference}");
                let reachable = self.native.as_ref().is_some_and(|f| f.closure(&self.root_workflow).is_some_and(|c| c.contains(&alias))) || context.layers.iter().any(|l| l.file.closure(&l.workflow).is_some_and(|c| c.contains(&alias)));
                if exact.is_some() && alias != key || exact.is_none() && !reachable { continue; }
                if let Some(pin) = self.accepted(context, &alias)? {
                    let kiln::Dependency::Image(image) = pin else { return Err(format!("KCI4054 {key}: its accepted pin is not an image")); };
                    let pin = kiln_ci::lock::execution::pin_image(reference, &image.digest)?;
                    accepted.merge(&kiln_ci::lock::execution::Locks { images: [(reference.into(), pin)].into(), ..Default::default() })?;
                }
            }
        }
        let old = self.image_locks.images.get(reference).cloned();
        if let Some(pin) = &old { accepted.merge(&kiln_ci::lock::execution::Locks { images: [(reference.into(), pin.clone())].into(), ..Default::default() })?; }
        let selected = accepted.images.get(reference).cloned().unwrap_or_else(|| reference.to_owned());
        if let Some(pin) = old {
            let digest = pin.split_once('@').map(|(_, d)| d).ok_or_else(|| format!("KCI4051 {reference}: image digest is missing"))?.to_owned();
            self.add(&key, kiln::Dependency::Image(kiln::Image { digest }))?;
            return Ok(key);
        }
        let manifest = match self.manifests.get(&selected) {
            Some(manifest) => manifest.clone(),
            None => {
                let manifest = self.source.manifest(selected.clone()).await.map_err(|e| format!("KCI4051 {key}: {e}"))?;
                self.count(manifest.bytes.len())?;
                self.manifests.insert(selected.clone(), manifest.clone());
                manifest
            }
        };
        manifest.validate(&selected).map_err(|e| format!("KCI4051 {key}: {e}"))?;
        let pinned = kiln_ci::lock::execution::pin_image(reference, &manifest.digest)?;
        self.image_locks.merge(&kiln_ci::lock::execution::Locks { images: [(reference.into(), pinned)].into(), ..Default::default() })?;
        self.add(&key, kiln::Dependency::Image(kiln::Image { digest: manifest.digest }))?;
        Ok(key)
    }

    async fn runtime(&mut self, using: &str, context: &Context) -> Result<String, String> {
        let key = using.to_owned();
        let accepted = self.accepted(context, using)?.or(self.accepted(context, &format!("runtime://{using}"))?);
        if self.mode == regenerate::Mode::Preserve && let Some(kiln::Dependency::Runtime(mut runtime)) = accepted {
            for (platform, expected) in &runtime.archives {
                let mut parts = platform.split('/');
                let (os, arch, musl) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next() == Some("musl"));
                let canonical = match using { "node12" | "node16" => "node20", _ => using };
                let request = Runtime { using: canonical.into(), os: os.into(), arch: arch.into(), musl, version: Some(expected.version.clone()) };
                let archive = self.source.runtime(request.clone()).await.map_err(|e| format!("KCI4051 {using}/{platform}: {e}"))?;
                archive.validate(&request).map_err(|e| format!("KCI4051 {using}/{platform}: {e}"))?;
                if archive.sha256 != expected.sha256 || archive.url != expected.url || archive.file != expected.file || archive.version != expected.version { return Err(format!("KCI4051 {using}/{platform}: exact runtime metadata disagrees with its accepted lock")); }
                self.count(archive.url.len().saturating_add(archive.file.len()))?;
            }
            if runtime.archives.is_empty() {
                let wanted = runtime.sha256.as_deref().ok_or_else(|| format!("KCI4050 {using}: accepted runtime has no checksum"))?;
                let canonical = match using { "node12" | "node16" => "node20", _ => using };
                for (os, arch, musl) in [("linux", "amd64", false), ("linux", "arm64", false), ("linux", "arm", false), ("macos", "amd64", false), ("macos", "arm64", false), ("linux", "amd64", true), ("linux", "arm64", true)] {
                    let request = Runtime { using: canonical.into(), os: os.into(), arch: arch.into(), musl, version: None };
                    let archive = match self.source.runtime(request.clone()).await {
                        Ok(archive) => archive,
                        Err(Refusal::Unsupported(_) | Refusal::Missing(_)) => continue,
                        Err(e) => return Err(format!("KCI4051 {using}/{os}/{arch}: {e}")),
                    };
                    archive.validate(&request).map_err(|e| format!("KCI4051 {using}/{os}/{arch}: {e}"))?;
                    if archive.sha256 != wanted { continue; }
                    self.count(archive.url.len().saturating_add(archive.file.len()))?;
                    runtime.archives.insert(format!("{os}/{arch}{}", if musl { "/musl" } else { "" }), kiln::Archive { version: archive.version, url: archive.url, file: archive.file, sha256: archive.sha256 });
                }
                if runtime.archives.is_empty() { return Err(format!("KCI4050 {using}: no versioned archive verifies its accepted checksum")); }
                runtime.sha256 = None;
            }
            self.add(&key, kiln::Dependency::Runtime(runtime))?;
            return Ok(key);
        }
        let canonical = match using { "node12" | "node16" => "node20", _ => using };
        let mut archives = BTreeMap::new();
        for (os, arch, musl) in [("linux", "amd64", false), ("linux", "arm64", false), ("linux", "arm", false), ("macos", "amd64", false), ("macos", "arm64", false), ("linux", "amd64", true), ("linux", "arm64", true)] {
            let request = Runtime { using: canonical.into(), os: os.into(), arch: arch.into(), musl, version: None };
            let archive = match self.source.runtime(request.clone()).await {
                Ok(archive) => archive,
                Err(Refusal::Unsupported(_) | Refusal::Missing(_)) => continue,
                Err(e) => return Err(format!("KCI4051 {using}/{os}/{arch}: {e}")),
            };
            archive.validate(&request).map_err(|e| format!("KCI4051 {using}: {e}"))?;
            self.count(archive.url.len().saturating_add(archive.file.len()))?;
            archives.insert(format!("{os}/{arch}{}", if musl { "/musl" } else { "" }), kiln::Archive { version: archive.version, url: archive.url, file: archive.file, sha256: archive.sha256 });
        }
        if archives.is_empty() { return Err(format!("KCI4051 {using}: no supported platform has a verified runtime archive")); }
        self.add(&key, kiln::Dependency::Runtime(kiln::Runtime { sha256: None, archives }))?;
        Ok(key)
    }

    async fn action(&mut self, uses: &str, context: &Context, depth: usize) -> Result<Vec<String>, String> {
        if depth > 10 { return Err(format!("KCI1022 {uses}: composite actions nest at most 10 deep")); }
        if crate::runner::builtins::is_kiln(uses) {
            return if crate::runner::builtins::builtin(uses).is_some() { Ok(Vec::new()) } else { Err(format!("KCI4050 {uses}: unknown kiln built-in")) };
        }
        if uses.starts_with("docker://") { return Ok(vec![self.image(uses, context).await?]); }
        let (pin, tree, base) = if let Some(base) = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/")) {
            (None, self.root.clone(), base.trim_matches('/').to_owned())
        } else {
            let (pin, tree, remote) = self.git(uses, context).await?;
            (Some(pin), tree, remote.path)
        };
        let marker = format!("{:?}:{}:{}:{base}", tree.owner.revision.as_ref().map(|r| &r.identity), tree.owner.repository, tree.owner.commit);
        if !self.active.insert(marker.clone()) { return Err(format!("KCI1022 {uses}: action dependency cycle")); }
        let mut text = None;
        for name in ["action.yml", "action.yaml"] {
            let path = if base.is_empty() { name.into() } else { format!("{base}/{name}") };
            if let Some(found) = self.file(&tree, &path).await? { text = Some(found); break; }
        }
        let text = text.ok_or_else(|| format!("KCI4051 {uses}: action metadata is missing at its selected commit"))?;
        let (action, ds) = wf::parse_action(&text);
        if ds.iter().any(wf::diag::Diag::is_error) { return Err(diagnostics(uses, ds)); }
        let action = action.ok_or_else(|| format!("KCI1022 {uses}: action metadata is invalid"))?;
        let mut edges = BTreeSet::new();
        if action.runs.using.starts_with("node") { edges.insert(self.runtime(&action.runs.using, context).await?); }
        if matches!(action.runs.using.as_str(), "docker" | "firecracker" | "shards") {
            let image = action.runs.image.as_deref().ok_or_else(|| format!("KCI1022 {uses}: action image is missing"))?;
            if image.starts_with("docker://") { edges.insert(self.image(image, context).await?); }
            else if action.runs.using != "docker" { edges.insert(self.image(&format!("microvm://{image}"), context).await?); }
            else {
                let relative = image.strip_prefix("./").unwrap_or(image);
                let path = if base.is_empty() { relative.into() } else { format!("{base}/{relative}") };
                let file = self.file(&tree, &path).await?.ok_or_else(|| format!("KCI4051 {uses}: Dockerfile is missing"))?;
                for image in dockerfile::images(&file).map_err(|ds| diagnostics(&path, ds))? { edges.insert(self.image(&format!("image://{image}"), context).await?); }
            }
        }
        let mut nested = Vec::new();
        step_uses(&action.runs.steps, &mut nested, 0, None);
        for (uses, _, _) in nested { edges.extend(Box::pin(self.action(&uses, context, depth.saturating_add(1))).await?); }
        self.active.remove(&marker);
        let edges: Vec<_> = edges.into_iter().collect();
        if let Some(mut pin) = pin {
            pin.uses = edges;
            self.add(uses, kiln::Dependency::Git(pin))?;
            Ok(vec![kiln::canonical_key(uses).ok_or_else(|| format!("KCI4050 {uses}: invalid source key"))?])
        } else { Ok(edges) }
    }

    async fn model(&mut self, tree: &Tree, path: &str) -> Result<Workflow, String> {
        let text = self.file(tree, path).await?.ok_or_else(|| format!("KCI4051 {path}: workflow is missing"))?;
        let dialect = if path.starts_with(".kiln/") { kiln_ci::expr::Dialect::Kiln } else { kiln_ci::expr::Dialect::Gha };
        let (model, ds) = wf::parse_workflow(path, &text, dialect);
        if ds.iter().any(wf::diag::Diag::is_error) { return Err(diagnostics(path, ds)); }
        let mut model = model.ok_or_else(|| format!("KCI1022 {path}: workflow is invalid"))?;
        if tree.owner.remote.is_none() && dialect == kiln_ci::expr::Dialect::Gha {
            let stem = path.rsplit('/').next().and_then(|p| p.rsplit_once('.')).map(|p| p.0).unwrap_or_default();
            for extension in ["yaml", "yml"] {
                let overlay = format!(".kiln/overlays/{stem}.{extension}");
                if let Some(text) = self.file(tree, &overlay).await? {
                    let (parsed, mut ds) = wf::parse_overlay(&text);
                    if let Some(parsed) = parsed { ds.extend(wf::apply_overlay(&mut model, &parsed)); }
                    if ds.iter().any(wf::diag::Diag::is_error) { return Err(diagnostics(&overlay, ds)); }
                    break;
                }
            }
        }
        Ok(model)
    }

    async fn workflow(&mut self, tree: &Tree, path: &str, inherited: &[native::Layer], prefix: &str, depth: usize) -> Result<Vec<String>, String> {
        if depth > 10 { return Err(format!("KCI1022 {path}: reusable workflows nest at most 10 deep")); }
        let marker = format!("{:?}:{}:{}:{path}", tree.owner.revision.as_ref().map(|r| &r.identity), tree.owner.repository, tree.owner.commit);
        if !self.active.insert(marker.clone()) { return Err(format!("KCI1022 {path}: reusable workflow dependency cycle")); }
        let model = self.model(tree, path).await?;
        if depth == 0 { self.root_name = model.name.clone().unwrap_or_default(); }
        let context = self.context(tree, path, inherited).await?;
        let mut roots = BTreeSet::new();
        let defaults = model.jobs.iter().map(|j| (j.id.clone(), crate::pipeline::toolchain_image(j.kiln.toolchain.as_deref()).to_owned())).collect();
        let settings = kiln_ci::plan::Settings { default_runs_on: vec!["k8s".into()], ..Default::default() };
        let route = regenerate::ImageContext { repo: &self.caller.name, workflow: &self.root_name, job_prefix: prefix };
        for image in regenerate::images(&model, route, &self.rules, &self.pools, &defaults, &settings).map_err(|ds| diagnostics(path, ds))? { roots.insert(self.image(&image, &context).await?); }
        for job in &model.jobs {
            let mut steps = Vec::new();
            step_uses(&job.steps, &mut steps, 0, None);
            for (uses, _, _) in steps { roots.extend(self.action(&uses, &context, 0).await?); }
            if let Some(uses) = &job.uses {
                let prefix = format!("{prefix}{}/", job.id);
                if let Some(path) = uses.strip_prefix("./").or_else(|| uses.strip_prefix("$/")) {
                    roots.extend(Box::pin(self.workflow(tree, path, &context.layers, &prefix, depth.saturating_add(1))).await?);
                } else {
                    let (mut pin, callee, remote) = self.git(uses, &context).await?;
                    pin.uses = Box::pin(self.workflow(&callee, &remote.path, &context.layers, &prefix, depth.saturating_add(1))).await?;
                    self.add(uses, kiln::Dependency::Git(pin))?;
                    roots.insert(kiln::canonical_key(uses).ok_or_else(|| format!("KCI4050 {uses}: invalid workflow source key"))?);
                }
            }
        }
        self.active.remove(&marker);
        Ok(roots.into_iter().collect())
    }
}

/// `paths` names touched workflows; None locks every workflow. The supplied edits overlay
/// their base for metadata and local action reads, without creating a draft or changing a ref.
pub(super) async fn collect(hub: &Hub, source: &dyn DependencySources, caller: &Repo, commit: &str, edits: BTreeMap<String, Option<Vec<u8>>>, paths: Option<&[String]>, mode: regenerate::Mode) -> Result<regenerate::Files, String> {
    if edits.len() > MAX_ITEMS || edits.values().flatten().fold(0usize, |n, bytes| n.saturating_add(bytes.len())) > MAX_BYTES { return Err("KCI1022 draft inputs exceed 16 MiB or 4096 files".into()); }
    let root = Tree { owner: Owner { repository: caller.name.clone(), commit: commit.into(), remote: None, revision: None }, edits: Arc::new(edits) };
    let rules = super::super::routing::sources(hub).await.effective();
    let pools = super::super::pools::sources(hub).await.0;
    let mut collector = Collector { hub, caller, source, root: root.clone(), root_workflow: String::new(), root_name: String::new(), github: None, native: None, revisions: BTreeMap::new(), resolutions: BTreeMap::new(), texts: BTreeMap::new(), rules, pools, image_locks: kiln_ci::lock::execution::Locks::default(), manifests: BTreeMap::new(), active: BTreeSet::new(), bytes: 0, items: 0, mode };
    let home = root.owner.home_host(hub, caller);
    let github = collector.file(&root, github::PATH).await?.map(|s| github::parse(&s).map_err(|ds| diagnostics(github::PATH, ds))).transpose()?;
    let native = collector.file(&root, kiln::PATH).await?.map(|s| kiln::parse(&s).map_err(|ds| diagnostics(kiln::PATH, ds))).transpose()?;
    if let (Some(gh), Some(kl)) = (&github, &native) { regenerate::compare(gh, kl, &home).map_err(|ds| diagnostics(kiln::PATH, ds))?; }
    collector.github = github.clone().map(Arc::new);
    collector.native = native.clone().map(Arc::new);
    let all = paths.is_none();
    let paths = match paths {
        Some(paths) => paths.to_vec(),
        None => {
            let loaded = super::super::load::load(hub, caller, commit).await.map_err(|_| "KCI4051 workflow tree is unavailable")?;
            let paths: BTreeSet<_> = loaded.workflows.into_iter().map(|w| w.path).chain(root.edits.iter().filter(|(p, text)| text.is_some() && matches!(wf::file_kind(p), Some(wf::FileKind::GhaWorkflow | wf::FileKind::KilnWorkflow))).map(|(p, _)| p.clone())).filter(|p| root.edits.get(p).is_none_or(Option::is_some)).collect();
            paths.into_iter().collect()
        }
    };
    let present: BTreeSet<_> = paths.iter().cloned().collect();
    let mut workflows = Vec::new();
    for path in paths {
        collector.root_workflow = path.clone();
        collector.image_locks = kiln_ci::lock::execution::Locks::default();
        let uses = collector.workflow(&root, &path, &[], "", 0).await?;
        workflows.push(regenerate::Workflow { path, uses: Some(uses) });
    }
    if all {
        for path in github.iter().flat_map(|f| f.workflows.keys()).chain(native.iter().flat_map(|f| f.workflows.keys())).collect::<BTreeSet<_>>() {
            if !present.contains(path) { workflows.push(regenerate::Workflow { path: path.clone(), uses: None }); }
        }
    }
    let resolutions: Vec<_> = collector.resolutions.into_iter().map(|(key, dependency)| regenerate::Resolution { key, dependency }).collect();
    regenerate::regenerate(regenerate::Input { github: github.as_ref(), kiln: native.as_ref(), workflows: &workflows, resolutions: &resolutions, seeds: &[], home_host: &home, mode }).map_err(|ds| diagnostics(kiln::PATH, ds))
}
