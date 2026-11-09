//! W29-R23: the dependency-source seam over kiln's existing git mirrors, host credentials,
//! registry integrations and Node publisher metadata. Refusals deliberately contain no raw
//! transport errors: a git URL, authorization header or signed storage URL may contain secrets.

use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::DependencySources;
use kiln_seams::dependencies::{self as dep, Archive, Identity, Manifest, Refusal, Repository, Revision, Runtime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{actions::{self, Remote}, integrations::{self, oci}};
use crate::http::client::Method;
use crate::hub::Hub;

const WITHIN: Duration = Duration::from_secs(60);

fn github_source(url: &str) -> Result<(String, String), Refusal> {
    let (host, name) = crate::githost::split_url(url).ok_or(Refusal::Unsupported("source host identity"))?;
    Repository { name: name.clone(), native: false }.validate().map_err(|_| Refusal::Invalid("source repository"))?;
    Ok((host, name))
}

#[derive(Serialize, Deserialize)]
struct CachedIdentity { binding: String, identity: Identity }

/// One preflight's source reader, shared while it visits dependencies. Its identity cache is
/// bounded like the simulator's; saved run snapshots contain the plain facts, not this reader.
pub struct Sources<H = Arc<Hub>> {
    hub: H,
    caller: String,
    identities: Mutex<BTreeMap<Repository, Option<Identity>>>,
    #[cfg(test)]
    endpoints: Option<TestEndpoints>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestEndpoints { host: String, api: String, registry: String, node: String }

/// A test's fake hosts, shared with the readers its server makes for itself (a draft's, the
/// dispatcher's): only for the hub they were shared for, while it lives.
#[cfg(test)]
static SHARED: Mutex<Vec<(std::sync::Weak<Hub>, TestEndpoints)>> = Mutex::new(Vec::new());

#[cfg(test)]
fn shared(hub: &Hub) -> Option<TestEndpoints> {
    let mut all = SHARED.lock();
    all.retain(|(h, _)| h.strong_count() > 0);
    all.iter().find(|(h, _)| std::ptr::eq(h.as_ptr(), hub)).map(|(_, e)| e.clone())
}

impl Sources {
    pub fn new(hub: Arc<Hub>, caller: String) -> Self {
        #[cfg(test)]
        let endpoints = shared(&hub);
        Self { hub, caller, identities: Mutex::default(), #[cfg(test)] endpoints }
    }
}

impl<'a> Sources<&'a Hub> {
    /// W29-R25: a replay's reader borrows its server and keeps no task past that read.
    pub(super) fn borrowed(hub: &'a Hub, caller: String) -> Self {
        Self { hub, caller, identities: Mutex::default(), #[cfg(test)] endpoints: shared(hub) }
    }
}

impl<H: Deref<Target = Hub> + Send + Sync> Sources<H> {
    fn remote(&self, repository: &Repository, reference: &str) -> Result<Remote, Refusal> {
        repository.validate()?;
        if !dep::reference_valid(reference) { return Err(Refusal::Invalid("ref")); }
        let (owner, repo) = repository.name.split_once('/').ok_or(Refusal::Invalid("repository name"))?;
        Ok(Remote { owner: owner.into(), repo: repo.into(), path: String::new(), git_ref: reference.into(), native: repository.native })
    }

    fn reserve(&self, repository: &Repository) -> Result<(), Refusal> {
        let mut ids = self.identities.lock();
        let key = repository.canonical();
        if !ids.contains_key(&key) && ids.len() >= dep::SIM_REPOSITORIES { return Err(Refusal::Full { what: "repositories", limit: dep::SIM_REPOSITORIES }); }
        ids.entry(key).or_default();
        Ok(())
    }

    async fn identity(&self, repository: &Repository, remote: &Remote) -> Result<(Identity, bool), Refusal> {
        self.reserve(repository)?;
        if !repository.name.eq_ignore_ascii_case(&self.caller) && kiln_ci::drafts::is_draft_ref(&remote.git_ref) { return Err(Refusal::Denied); }
        remote.usable_by(&self.hub, &self.caller).await.map_err(|_| Refusal::Denied)?;
        let native = remote.native || self.hub.cfg.ci.actions.source_of(&remote.owner).is_some_and(|s| s.native);
        let binding = if native { format!("kiln:{}", repository.name.to_ascii_lowercase()) } else {
            let (host, name) = self.github_location(remote)?;
            format!("github:{host}/{}", name.to_ascii_lowercase())
        };
        let result = if native {
            self.hub.store.repos().await.map_err(|_| Refusal::Unavailable("repository identity"))?.into_iter()
                .find(|r| r.mode == "native" && r.name.eq_ignore_ascii_case(&repository.name))
                .map(|r| Identity::Native { repo_id: r.id }).ok_or(Refusal::Missing("repository"))
        } else { self.github_identity(remote).await };
        match result {
            Ok(identity) => {
                identity.validate()?;
                let unchanged = self.identities.lock().get(&repository.canonical()).and_then(Option::as_ref) == Some(&identity);
                if !unchanged { self.keep_identity(remote, binding, identity.clone()).await?; }
                self.identities.lock().insert(repository.canonical(), Some(identity.clone()));
                Ok((identity, true))
            }
            Err(Refusal::Unavailable(_)) => {
                // W29-R23: numeric identities stay beside the mirror. A fresh reader can use
                // an offline mirror, but a changed source mapping cannot borrow its identity.
                let memory = self.identities.lock().get(&repository.canonical()).and_then(Clone::clone);
                let kept = match memory { Some(id) => Some(id), None => self.cached_identity(remote, &binding).await? };
                if let Some(identity) = &kept { self.identities.lock().insert(repository.canonical(), Some(identity.clone())); }
                kept.map(|i| (i, false)).ok_or(Refusal::Unavailable("repository identity"))
            }
            Err(e) => Err(e),
        }
    }

    fn github_location(&self, remote: &Remote) -> Result<(String, String), Refusal> {
        let url = actions::source(&self.hub, &remote.owner, &remote.repo).map_err(|_| Refusal::Unavailable("source configuration"))?;
        let source = github_source(&url);
        #[cfg(test)]
        let source = if let Some(e) = &self.endpoints { Ok((e.host.clone(), format!("{}/{}", remote.owner, remote.repo))) } else { source };
        source
    }

    async fn keep_identity(&self, remote: &Remote, binding: String, identity: Identity) -> Result<(), Refusal> {
        let path = remote.mirror(&self.hub).with_extension("identity.json");
        let bytes = serde_json::to_vec(&CachedIdentity { binding, identity }).map_err(|_| Refusal::Invalid("repository identity"))?;
        let staging = path.with_extension(format!("{}.pending", crate::util::new_id()));
        crate::rt::blocking(move || -> std::io::Result<()> {
            if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
            let result = std::fs::write(&staging, &bytes).and_then(|()| std::fs::rename(&staging, &path));
            if result.is_err() { let _ = std::fs::remove_file(staging); }
            result
        }).await.map_err(|_| Refusal::Unavailable("identity cache"))?.map_err(|_| Refusal::Unavailable("identity cache"))
    }

    async fn cached_identity(&self, remote: &Remote, binding: &str) -> Result<Option<Identity>, Refusal> {
        let path = remote.mirror(&self.hub).with_extension("identity.json");
        let bytes = crate::rt::blocking(move || -> std::io::Result<Option<Vec<u8>>> {
            let mut file = match std::fs::File::open(path) { Ok(f) => f, Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), Err(e) => return Err(e) };
            // Read at most 8 KiB plus the overflow byte, including when a file changes during
            // the read. The cache is adapter-private, never an untrusted workspace's file.
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, 8193), &mut bytes)?;
            Ok(Some(bytes))
        }).await.map_err(|_| Refusal::Unavailable("identity cache"))?.map_err(|_| Refusal::Unavailable("identity cache"))?;
        let Some(bytes) = bytes else { return Ok(None) };
        if bytes.len() > 8192 { return Err(Refusal::TooLarge { what: "identity cache", limit: 8192 }); }
        let cached: CachedIdentity = serde_json::from_slice(&bytes).map_err(|_| Refusal::Invalid("identity cache"))?;
        cached.identity.validate()?;
        Ok((cached.binding == binding).then_some(cached.identity))
    }

    async fn github_identity(&self, remote: &Remote) -> Result<Identity, Refusal> {
        let (hostname, name) = self.github_location(remote)?;
        let app = crate::githost::github::app(&self.hub).await.filter(|a| crate::githost::host_of(&a.web) == hostname);
        let api = app.as_ref().map(|a| a.api.clone()).or_else(|| (hostname == "github.com").then(|| "https://api.github.com".into()));
        #[cfg(test)]
        let api = self.endpoints.as_ref().map(|e| e.api.clone()).or(api);
        let api = api.ok_or(Refusal::Unsupported("source host identity"))?;
        // Installation credentials only go to the App's own API. An installation that doesn't
        // include a public action repository need not grant access to that public repository.
        let token = match &app { Some(a) => a.token(&name, &[("metadata".into(), "read".into())].into()).await.ok(), None => None };
        // Servers using gitops' GitHub credential rather than an App need the same access for
        // repository identity. That github.com credential is never sent to another host.
        let token = match (token, hostname.as_str(), self.hub.cfg.gitops.github_token_file.as_ref()) {
            (None, "github.com", Some(file)) => crate::githost::read_secret(file).await.map_err(|_| Refusal::Unavailable("source credential"))?,
            (token, _, _) => token,
        };
        let http = crate::http::client::github().map_err(|_| Refusal::Unavailable("HTTP client"))?;
        let mut request = http.get(&format!("{api}/repos/{name}")).header("Accept", "application/vnd.github+json").header("User-Agent", "kiln").timeout(WITHIN);
        if let Some(token) = &token { request = request.bearer(token); }
        let response = request.send().await.map_err(|_| Refusal::Unavailable("repository identity"))?;
        match response.status() {
            200 => {},
            401 | 403 => return Err(Refusal::Denied),
            404 => return Err(Refusal::Missing("repository")),
            429 | 500..=599 => return Err(Refusal::Unavailable("repository identity")),
            _ => return Err(Refusal::Invalid("repository identity response")),
        }
        let value: Value = response.json(dep::FILE_MAX).await.map_err(|_| Refusal::Invalid("repository identity"))?;
        let identity = Identity::Github {
            hostname,
            owner_id: value.pointer("/owner/id").and_then(Value::as_i64).ok_or(Refusal::Invalid("repository identity"))?,
            repo_id: value.get("id").and_then(Value::as_i64).ok_or(Refusal::Invalid("repository identity"))?,
        };
        identity.validate()?;
        Ok(identity)
    }

    async fn checked(&self, revision: &Revision) -> Result<Remote, Refusal> {
        revision.validate()?;
        let remote = self.remote(&revision.repository, &revision.requested)?;
        let (identity, _) = self.identity(&revision.repository, &remote).await?;
        if identity != revision.identity { return Err(Refusal::IdentityChanged); }
        self.eligible(&remote, &revision.commit).await?;
        Ok(remote)
    }

    async fn eligible(&self, remote: &Remote, commit: &str) -> Result<(), Refusal> {
        let by_commit = !remote.git_ref.is_empty() && remote.git_ref.bytes().all(|b| b.is_ascii_hexdigit()) && commit.to_ascii_lowercase().starts_with(&remote.git_ref.to_ascii_lowercase());
        let native = remote.native || self.hub.cfg.ci.actions.source_of(&remote.owner).is_some_and(|s| s.native);
        if by_commit && native && !format!("{}/{}", remote.owner, remote.repo).eq_ignore_ascii_case(&self.caller) {
            let _guard = actions::source_lock(remote).map_err(|_| Refusal::Unavailable("mirror lock"))?.read().await;
            let refs = self.bytes(remote, &["for-each-ref", "--contains", commit, "--format=%(refname)", "refs/heads", "refs/tags"], "publication history").await?;
            if !String::from_utf8_lossy(&refs).lines().any(|r| !r.is_empty() && !kiln_ci::drafts::is_draft_ref(r)) { return Err(Refusal::Denied); }
        }
        Ok(())
    }

    async fn git(&self, remote: &Remote, args: &[&str]) -> Result<std::process::Output, Refusal> {
        let mut cmd = actions::source_command(&self.hub, remote).await;
        cmd.env("GIT_NO_REPLACE_OBJECTS", "1").arg("-C").arg(remote.mirror(&self.hub)).args(args);
        crate::proc::output(cmd, WITHIN).await.map_err(|_| Refusal::Unavailable("git mirror"))
    }

    async fn bytes(&self, remote: &Remote, args: &[&str], what: &'static str) -> Result<Vec<u8>, Refusal> {
        let out = self.git(remote, args).await?;
        if !out.status.success() { return Err(Refusal::Missing(what)); }
        Ok(out.stdout)
    }

    async fn contains_at(&self, remote: &Remote, revision: &Revision, commit: &str) -> Result<bool, Refusal> {
        if !dep::commit_valid(commit) { return Err(Refusal::Invalid("commit")); }
        self.bytes(remote, &["cat-file", "-e", &format!("{}^{{commit}}", revision.commit)], "captured commit").await?;
        self.bytes(remote, &["cat-file", "-e", &format!("{commit}^{{commit}}")], "commit").await?;
        let result = self.git(remote, &["merge-base", "--is-ancestor", commit, &revision.commit]).await?;
        match result.status.code() {
            Some(0) => Ok(true),
            Some(1) => {
                let shallow = self.bytes(remote, &["rev-parse", "--is-shallow-repository"], "mirror").await?;
                if shallow.starts_with(b"true") { return Err(Refusal::Unavailable("commit history")); }
                Ok(false)
            }
            _ => Err(Refusal::Unavailable("commit history")),
        }
    }
}

impl<H: Deref<Target = Hub> + Send + Sync> DependencySources for Sources<H> {
    fn compare(&self, before: Revision, after: Revision) -> BoxFuture<'_, Result<dep::Difference, Refusal>> {
        Box::pin(compare::compare(self, before, after))
    }

    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>> {
        Box::pin(async move {
            let remote = self.remote(&repository, &reference)?;
            let (identity, identity_fresh) = self.identity(&repository, &remote).await?;
            let (commit, full, fresh) = actions::pin_ref_readonly(&self.hub, &remote, identity_fresh).await.map_err(|_| {
                if remote.mirror(&self.hub).join("HEAD").exists() && identity_fresh { Refusal::Missing("ref") } else { Refusal::Unavailable("mirror") }
            })?;
            self.eligible(&remote, &commit).await?;
            let revision = Revision { repository, identity, requested: reference, reference: full, commit, fresh: fresh && identity_fresh };
            revision.validate()?;
            Ok(revision)
        })
    }

    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>> {
        Box::pin(async move {
            let remote = self.checked(&revision).await?;
            let _guard = actions::source_lock(&remote).map_err(|_| Refusal::Unavailable("mirror lock"))?.read().await;
            self.contains_at(&remote, &revision, &commit).await
        })
    }

    fn tags_at(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<Vec<String>, Refusal>> {
        Box::pin(async move {
            if !dep::commit_valid(&commit) { return Err(Refusal::Invalid("commit")); }
            let remote = self.checked(&revision).await?;
            let _guard = actions::source_lock(&remote).map_err(|_| Refusal::Unavailable("mirror lock"))?.read().await;
            self.bytes(&remote, &["cat-file", "-e", &format!("{commit}^{{commit}}")], "commit").await?;
            // An annotated tag points at the commit it tags, as `git tag --points-at` reads it.
            let refs = self.bytes(&remote, &["for-each-ref", "--points-at", &commit, "--format=%(refname)", "refs/tags"], "tags").await?;
            let mut tags: Vec<String> = String::from_utf8_lossy(&refs).lines().filter(|r| r.starts_with("refs/tags/") && dep::reference_valid(r)).map(str::to_owned).collect();
            tags.sort();
            tags.truncate(dep::TAGS_MAX);
            Ok(tags)
        })
    }

    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>> {
        Box::pin(async move {
            if !dep::path_valid(&path) { return Err(Refusal::Invalid("file path")); }
            let remote = self.checked(&revision).await?;
            let _guard = actions::source_lock(&remote).map_err(|_| Refusal::Unavailable("mirror lock"))?.read().await;
            if !self.contains_at(&remote, &revision, &commit).await? { return Err(Refusal::NotReachable); }
            // Resolve the blob once; its immutable size is checked before cat-file copies it.
            let object = self.bytes(&remote, &["rev-parse", "--verify", &format!("{commit}:{path}")], "file").await?;
            let object = String::from_utf8_lossy(&object).trim().to_owned();
            if !dep::commit_valid(&object) { return Err(Refusal::Invalid("file object")); }
            let kind = self.bytes(&remote, &["cat-file", "-t", &object], "file").await?;
            if kind != b"blob\n" { return Err(Refusal::Invalid("file object")); }
            let size = self.bytes(&remote, &["cat-file", "-s", &object], "file").await?;
            let size = String::from_utf8_lossy(&size).trim().parse::<u64>().map_err(|_| Refusal::Invalid("file size"))?;
            if size > u64::try_from(dep::FILE_MAX).unwrap_or(u64::MAX) { return Err(Refusal::TooLarge { what: "file", limit: dep::FILE_MAX }); }
            let bytes = self.bytes(&remote, &["cat-file", "blob", &object], "file").await?;
            if bytes.len() > dep::FILE_MAX { return Err(Refusal::TooLarge { what: "file", limit: dep::FILE_MAX }); }
            if u64::try_from(bytes.len()).ok() != Some(size) { return Err(Refusal::Integrity("file")); }
            Ok(bytes)
        })
    }

    fn manifest(&self, reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>> {
        Box::pin(async move {
            if !dep::image_valid(&reference) { return Err(Refusal::Invalid("image reference")); }
            let r = oci::parse_reference(&reference).map_err(|_| Refusal::Invalid("image reference"))?;
            let registry = integrations::registry_for(&self.hub, &r.registry).await.map_err(|_| Refusal::Unavailable("registry integration"))?.unwrap_or_else(|| oci::Registry::for_name(&r.registry, None, false));
            #[cfg(test)]
            let registry = if let Some(e) = &self.endpoints { oci::Registry { base: e.registry.clone(), ..registry } } else { registry };
            let target = r.digest.as_deref().or(r.tag.as_deref()).unwrap_or("latest");
            let mut response = oci::call(&registry, Method::Get, &format!("/v2/{}/manifests/{target}", r.repository), &format!("repository:{}:pull", r.repository)).await.map_err(|_| Refusal::Unavailable("registry"))?;
            match response.status() {
                200 => {}, 401 | 403 => return Err(Refusal::Denied), 404 => return Err(Refusal::Missing("manifest")), _ => return Err(Refusal::Unavailable("registry")),
            }
            let media_type = response.header("content-type").unwrap_or("application/vnd.oci.image.manifest.v1+json").split(';').next().unwrap_or_default().trim().to_owned();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Refusal::Unavailable("manifest"))? {
                if bytes.len().saturating_add(chunk.len()) > dep::MANIFEST_MAX { return Err(Refusal::TooLarge { what: "manifest", limit: dep::MANIFEST_MAX }); }
                bytes.extend_from_slice(&chunk);
            }
            let manifest = Manifest { digest: format!("sha256:{}", kiln_seams::sha256_hex(&bytes)), media_type, bytes };
            manifest.validate(&reference)?;
            Ok(manifest)
        })
    }

    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>> {
        Box::pin(async move {
            let major = runtime.major()?;
            let http = oci::client().map_err(|_| Refusal::Unavailable("HTTP client"))?;
            let arch = match runtime.arch.as_str() { "amd64" => "x64", "arm64" => "arm64", "arm" => "armv7l", _ => return Err(Refusal::Unsupported("runtime platform")) };
            let os = if runtime.os == "macos" { "darwin" } else { "linux" };
            #[cfg(test)]
            let fixture = if let Some(e) = &self.endpoints {
                let endpoint = match &runtime.version { Some(v) => format!("{}/{v}/SHASUMS256.txt", e.node.rsplit_once('/').map_or(e.node.as_str(), |(base, _)| base)), None => e.node.clone() };
                let response = http.get(&endpoint).timeout(WITHIN).send().await.map_err(|_| Refusal::Unavailable("runtime metadata"))?;
                if !response.is_success() { return Err(Refusal::Unavailable("runtime metadata")); }
                let sums = response.bytes(dep::FILE_MAX).await.map_err(|_| Refusal::Unavailable("runtime metadata"))?;
                Some(crate::runner::runtime::node_archive_listed(&String::from_utf8_lossy(&sums), major, arch, os))
            } else { None };
            #[cfg(not(test))]
            let fixture: Option<anyhow::Result<(String, String, String)>> = None;
            let result = match fixture { Some(r) => r, None => crate::runner::runtime::node_archive_at(http, major, runtime.musl, arch, os, runtime.version.as_deref()).await };
            let (url, file, sha256) = result.map_err(|_| Refusal::Unavailable("runtime metadata"))?;
            let version = file.strip_prefix("node-").and_then(|s| s.split_once('-')).map(|(v, _)| v.to_owned()).ok_or(Refusal::Invalid("runtime archive metadata"))?;
            let archive = Archive { version, url, file, sha256 };
            archive.validate(&runtime)?;
            Ok(archive)
        })
    }
}

mod compare;

#[cfg(all(test, unix))]
pub(crate) mod tests;
