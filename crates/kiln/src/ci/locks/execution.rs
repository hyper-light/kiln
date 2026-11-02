//! W29-R24: capture non-Git inputs through the source seam before a leg can be queued.

use kiln_ci::lock::{dockerfile, execution::Locks, kiln};
use kiln_seams::dependencies::{Archive, Runtime};

use super::*;

impl Capture<'_> {
    pub(super) async fn image(&mut self, reference: &str, native: &[native::Layer], into: &mut Locks) -> Result<(), String> {
        let reference = reference.strip_prefix("docker://").unwrap_or(reference);
        let mut selected = None;
        for layer in native {
            let reachable = layer.file.closure(&layer.workflow).unwrap_or_default();
            for prefix in ["docker://", "image://", "executor://", "microvm://"] {
                let raw = format!("{prefix}{reference}");
                let key = kiln::canonical_key(&raw).unwrap_or(raw);
                if !reachable.contains(&key) { continue; }
                if let Some(kiln::Dependency::Image(image)) = layer.file.dependencies.get(&key) {
                    let pinned = kiln_ci::lock::execution::pin_image(reference, &image.digest)?;
                    if selected.as_ref().is_some_and(|old| old != &pinned) { return Err(format!("KCI4054 {reference}: image roles select different digests")); }
                    selected = Some(pinned);
                }
            }
            if selected.is_some() { break; }
        }
        let pinned = selected.ok_or_else(|| self.missing(self.root_path, reference))?;
        self.allowed(&format!("docker://{pinned}"))?;
        if !self.manifests.contains(&pinned) {
            let manifest = self.source.manifest(pinned.clone()).await.map_err(|e| format!("KCI4051 {reference}: {e}"))?;
            manifest.validate(&pinned).map_err(|e| format!("KCI4051 {reference}: {e}"))?;
            self.count(manifest.bytes.len())?;
            self.manifests.insert(pinned.clone());
        }
        if into.images.get(reference).is_some_and(|old| old != &pinned) { return Err(format!("KCI4054 {reference}: execution image pins disagree")); }
        into.images.insert(reference.into(), pinned);
        Ok(())
    }

    async fn runtime_archive(&mut self, request: Runtime) -> Result<Archive, String> {
        if let Some(archive) = self.archives.get(&request) { return Ok(archive.clone()); }
        let archive = self.source.runtime(request.clone()).await.map_err(|e| format!("KCI4051 {}: {e}", request.using))?;
        archive.validate(&request).map_err(|e| format!("KCI4051 {}: {e}", request.using))?;
        self.count(archive.url.len().saturating_add(archive.file.len()))?;
        self.archives.insert(request, archive.clone());
        Ok(archive)
    }

    async fn runtime(&mut self, using: &str, native: &[native::Layer], into: &mut Locks) -> Result<(), String> {
        let canonical = match using { "node12" | "node16" => "node20", _ => using };
        let mut selected = None;
        for layer in native {
            let reachable = layer.file.closure(&layer.workflow).unwrap_or_default();
            for name in [using, canonical] {
                for key in [name.to_owned(), format!("runtime://{name}")] {
                    if reachable.contains(&key) && let Some(kiln::Dependency::Runtime(runtime)) = layer.file.dependencies.get(&key) {
                        if selected.as_ref().is_some_and(|old| old != runtime) { return Err(format!("KCI4054 {using}: runtime lock entries disagree")); }
                        selected = Some(runtime.clone());
                    }
                }
            }
            if selected.is_some() { break; }
        }
        let selected = selected.ok_or_else(|| self.missing(self.root_path, using))?;
        let mut archives = BTreeMap::new();
        for (platform, expected) in selected.archives {
            let mut parts = platform.split('/');
            let (os, arch, musl) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next() == Some("musl"));
            let actual = self.runtime_archive(Runtime { using: canonical.into(), os: os.into(), arch: arch.into(), musl, version: Some(expected.version.clone()) }).await?;
            let got = kiln::Archive { version: actual.version, url: actual.url, file: actual.file, sha256: actual.sha256 };
            if got != expected { return Err(format!("KCI4051 {using}/{platform}: exact runtime metadata disagrees with the lock")); }
            archives.insert(platform, got);
        }
        // The original R24 shorthand has no location. Establish one only where the source's
        // checksum agrees; a newer archive never becomes a replacement for the accepted hash.
        if archives.is_empty() && let Some(wanted) = selected.sha256 {
            for (os, arch, musl) in [("linux", "amd64", false), ("linux", "arm64", false), ("linux", "arm", false), ("macos", "amd64", false), ("macos", "arm64", false), ("linux", "amd64", true), ("linux", "arm64", true)] {
                let request = Runtime { using: canonical.into(), os: os.into(), arch: arch.into(), musl, version: None };
                let Ok(actual) = self.runtime_archive(request).await else { continue };
                if actual.sha256 != wanted { continue; }
                let platform = format!("{os}/{arch}{}", if musl { "/musl" } else { "" });
                archives.insert(platform, kiln::Archive { version: actual.version, url: actual.url, file: actual.file, sha256: actual.sha256 });
            }
        }
        if archives.is_empty() { return Err(format!("KCI4050 {using}: no versioned archive verifies the locked runtime checksum")); }
        into.runtimes.insert(using.into(), archives);
        Ok(())
    }

    pub(super) async fn action_execution(&mut self, uses: &str, action: &wf::Action, owner: &Owner, base: &str, native: &[native::Layer]) -> Result<Locks, String> {
        let mut locks = Locks::default();
        if native.is_empty() {
            if action.runs.using.starts_with("node") || matches!(action.runs.using.as_str(), "docker" | "firecracker" | "shards") {
                let why = format!("KCI4055 {uses}: Require lockfile: {} needs a supplemental runtime/image lock", action.runs.using);
                if self.lock_policy.require == verify::Require::Enforce { return Err(why); }
                if self.lock_policy.require == verify::Require::Report { self.warnings.insert(why); }
            }
            return Ok(locks);
        }
        if action.runs.using.starts_with("node") { self.runtime(&action.runs.using, native, &mut locks).await?; }
        if action.runs.using == "docker" {
            let image = action.runs.image.as_deref().ok_or_else(|| format!("KCI1022 {uses}: Docker action has no image"))?;
            if image.starts_with("docker://") { self.image(image, native, &mut locks).await?; }
            else {
                let relative = image.strip_prefix("./").unwrap_or(image);
                let path = if base.is_empty() { relative.into() } else { format!("{base}/{relative}") };
                if !kiln_seams::dependencies::path_valid(&path) { return Err(format!("KCI1022 {uses}: Dockerfile must stay in its action")); }
                let bytes = owner.file(self.hub, self.source, &path).await?.ok_or_else(|| format!("KCI4051 {uses}: Dockerfile is missing at its selected commit"))?;
                self.count(bytes.len())?;
                let source_sha256 = crate::util::sha256_hex(&bytes);
                let text = std::str::from_utf8(&bytes).map_err(|_| format!("KCI1022 {uses}: Dockerfile is not UTF-8"))?;
                let diagnostics = |ds: Vec<wf::diag::Diag>| ds.iter().map(|d| d.render(&path)).collect::<Vec<_>>().join("\n");
                for image in dockerfile::images(text).map_err(diagnostics)? { self.image(&image, native, &mut locks).await?; }
                let locked = dockerfile::rewrite(text, &locks.images).map_err(diagnostics)?;
                locks.dockerfiles.insert(uses.into(), kiln_ci::lock::execution::Dockerfile { path: relative.into(), source_sha256, locked });
            }
        }
        locks.validate()?;
        Ok(locks)
    }

    pub(super) async fn job_execution(&mut self, workflow: &Workflow, native: &[native::Layer], assets: &BTreeMap<String, Locks>, children: &BTreeMap<String, Vec<String>>, prefix: &str) -> Result<BTreeMap<String, Locks>, String> {
        let mut jobs = BTreeMap::new();
        if native.is_empty() && self.lock_policy.require != verify::Require::Enforce {
            if self.lock_policy.require == verify::Require::Report && workflow.jobs.iter().any(|j| j.container.is_some() || !j.services.is_empty() || j.kiln.image.is_some() || j.kiln.toolchain.is_some()) { self.warnings.insert(format!("KCI4055 {}: Require lockfile: execution images need a supplemental lock", workflow.path)); }
            return Ok(jobs);
        }
        let rules = super::super::routing::sources(self.hub).await.effective();
        let pools = super::super::pools::sources(self.hub).await.0;
        for job in &workflow.jobs {
            if job.uses.is_some() { continue; }
            let mut locks = Locks::default();
            let mut one = workflow.clone(); one.jobs = vec![job.clone()];
            let defaults = [(job.id.clone(), crate::pipeline::toolchain_image(job.kiln.toolchain.as_deref()).to_owned())].into();
            let settings = kiln_ci::plan::Settings { default_runs_on: vec!["k8s".into()], ..Default::default() };
            let context = kiln_ci::lock::regenerate::ImageContext { repo: &self.caller.name, workflow: &self.root_name, job_prefix: prefix };
            let images = kiln_ci::lock::regenerate::images(&one, context, &rules, &pools, &defaults, &settings).map_err(|ds| {
                let mut why = ds.iter().map(|d| d.render(&workflow.path)).collect::<Vec<_>>().join("\n");
                if self.lock_policy.require == verify::Require::Enforce { why.push_str("\nKCI4055 Require lockfile: execution inputs cannot be proven covered"); }
                why
            })?;
            if native.is_empty() {
                if !images.is_empty() {
                    let why = format!("KCI4055 {}: Require lockfile: this job's images have no supplemental lock", job.id);
                    if self.lock_policy.require == verify::Require::Enforce { return Err(why); }
                    self.warnings.insert(why);
                }
                if self.lock_policy.require == verify::Require::Enforce { jobs.insert(job.id.clone(), locks); }
                continue;
            }
            for image in images {
                let reference = kiln::image_key(&image).ok_or_else(|| format!("KCI4050 {image}: image reference is invalid"))?;
                self.image(reference, native, &mut locks).await?;
            }
            for layer in native {
                for key in layer.file.closure(&layer.workflow).unwrap_or_default() {
                    if let Some(image) = key.strip_prefix("executor://").or_else(|| key.strip_prefix("microvm://")) {
                        self.image(image, native, &mut locks).await?;
                    }
                }
            }
            let mut todo = Vec::new();
            step_uses(&job.steps, &mut todo, 0, None);
            let mut seen = BTreeSet::new();
            while let Some((uses, _, _)) = todo.pop() {
                if !seen.insert(uses.clone()) { continue; }
                if let Some(asset) = assets.get(&uses) { locks.merge(asset)?; }
                if let Some(nested) = children.get(&uses) { todo.extend(nested.iter().map(|u| (u.clone(), 0, None))); }
            }
            // A locked shell-only job still needs enforcement if routing gives it an executor image.
            locks.validate()?; jobs.insert(job.id.clone(), locks);
        }
        Ok(jobs)
    }
}
