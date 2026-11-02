//! Dependency source facts for locks (docs/ci.md W29-R23, R26). The source owns credentials;
//! decisions about accepting these facts belong to kiln-ci. A captured revision is immutable:
//! history and files are read against its commit, even after its branch or tag moves.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub const NAME_MAX: usize = 256;
pub const PATH_MAX: usize = 4096;
pub const FILE_MAX: usize = 1 << 20;
pub const MANIFEST_MAX: usize = 4 << 20;
pub const DIFFERENCE_MAX: usize = 4 << 20;
pub const DIFFERENCE_FILES: usize = 4096;
pub const SIM_REPOSITORIES: usize = 256;
pub const SIM_COMMITS: usize = 4096;
pub const SIM_FILES: usize = 4096;
pub const SIM_REFS: usize = 1024;
pub const SIM_BYTES: usize = 64 << 20;
pub const SIM_METADATA: usize = 256;

/// An action source, using the same source mapping as `uses:`. Native sources are separate
/// from GitHub repositories of the same name. No URL or credential is accepted here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Repository {
    pub name: String,
    pub native: bool,
}

/// The host's stable identity, rather than a name someone can delete and take over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Identity {
    Github { hostname: String, owner_id: i64, repo_id: i64 },
    Native { repo_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub repository: Repository,
    pub identity: Identity,
    /// What was asked for, including `HEAD` when it named the default branch.
    pub requested: String,
    /// The qualified branch or tag, or the full commit when asked for by commit.
    pub reference: String,
    pub commit: String,
    /// False when only a previously fetched mirror could answer. This is not evidence of
    /// what the upstream ref names now.
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub digest: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Runtime {
    pub using: String,
    pub os: String,
    /// Go's architecture names: `amd64`, `arm64`, `arm`.
    pub arch: String,
    pub musl: bool,
    /// W29-R24: an accepted archive's exact version; None asks for a new resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// An immutable archive location and its publisher's SHA-256; downloading and checking the
/// archive happens later, on the runner. URLs have no credentials, query or fragment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    pub version: String,
    pub url: String,
    pub file: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("invalid dependency {0}")]
    Invalid(&'static str),
    #[error("dependency {0} is missing")]
    Missing(&'static str),
    #[error("dependency access was denied")]
    Denied,
    #[error("dependency {0} is unavailable")]
    Unavailable(&'static str),
    #[error("dependency repository identity changed")]
    IdentityChanged,
    #[error("the commit is outside the captured source history")]
    NotReachable,
    #[error("dependency {what} exceeds {limit} bytes")]
    TooLarge { what: &'static str, limit: usize },
    #[error("dependency {0} failed its integrity check")]
    Integrity(&'static str),
    #[error("dependency {0} is unsupported")]
    Unsupported(&'static str),
    #[error("dependency source holds its most ({limit}) {what}")]
    Full { what: &'static str, limit: usize },
}

/// Reads only: resolving a ref may refresh a private mirror, but never releases, updates a
/// host, or queues an update draft. Panes use their existing cached facts instead of this seam.
/// A reader may borrow its server for one operation. Returned facts own their data; a reader
/// kept by a background task still has to satisfy that task's lifetime.
pub trait DependencySources: Send + Sync {
    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>>;
    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>>;
    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>>;
    fn manifest(&self, reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>>;
    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>>;
    /// W29-R25: all changed source files, including binary bytes and mode changes. This is
    /// evidence for a review, never permission to accept either commit or run its code.
    fn compare(&self, _before: Revision, _after: Revision) -> BoxFuture<'_, Result<Difference, Refusal>> {
        Box::pin(async { Err(Refusal::Unsupported("code difference")) })
    }
}

/// Complete file contents let the existing review renderer show the whole code change,
/// including files outside action metadata. Symlinks are bytes, never followed; a gitlink's
/// bytes are its commit id. No external diff driver or text conversion supplies these facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeFile { pub mode: u32, #[serde(rename = "base64", with = "code_bytes")] pub bytes: Vec<u8> }

mod code_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() > super::DIFFERENCE_MAX.saturating_mul(4).saturating_div(3).saturating_add(4) { return Err(serde::de::Error::custom("code difference exceeds its byte bound")); }
        base64::engine::general_purpose::STANDARD.decode(text).map_err(|_| serde::de::Error::custom("invalid code bytes"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChange { pub path: String, pub before: Option<CodeFile>, pub after: Option<CodeFile> }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Difference { pub before: Revision, pub after: Revision, pub files: Vec<CodeChange> }

impl CodeFile {
    pub fn validate(&self) -> Result<(), Refusal> {
        if !matches!(self.mode, 0o100644 | 0o100755 | 0o120000 | 0o160000)
            || self.mode == 0o160000 && !std::str::from_utf8(&self.bytes).is_ok_and(commit_valid) { return Err(Refusal::Invalid("code file mode")); }
        Ok(())
    }
}

impl Difference {
    pub fn validate(&self) -> Result<(), Refusal> {
        self.before.validate()?;
        self.after.validate()?;
        if self.before.repository.canonical() != self.after.repository.canonical() { return Err(Refusal::Invalid("comparison repository")); }
        if self.before.identity != self.after.identity { return Err(Refusal::IdentityChanged); }
        if self.files.len() > DIFFERENCE_FILES { return Err(Refusal::Full { what: "changed files", limit: DIFFERENCE_FILES }); }
        let mut paths = BTreeSet::new();
        let mut size = 0usize;
        for change in &self.files {
            if !path_valid(&change.path) || !paths.insert(&change.path) || change.before == change.after { return Err(Refusal::Invalid("code change")); }
            size = size.saturating_add(change.path.len());
            for file in change.before.iter().chain(&change.after) { file.validate()?; size = size.saturating_add(file.bytes.len()); }
            if size > DIFFERENCE_MAX { return Err(Refusal::TooLarge { what: "code difference", limit: DIFFERENCE_MAX }); }
        }
        Ok(())
    }
}

fn name(s: &str) -> bool {
    !s.is_empty() && s.len() <= NAME_MAX && s.bytes().any(|b| b != b'.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl Repository {
    pub fn validate(&self) -> Result<(), Refusal> {
        match self.name.split_once('/') {
            Some((owner, repo)) if name(owner) && name(repo) => Ok(()),
            _ => Err(Refusal::Invalid("repository name")),
        }
    }

    pub fn canonical(&self) -> Self {
        Self { name: self.name.to_ascii_lowercase(), native: self.native }
    }
}

impl Identity {
    pub fn validate(&self) -> Result<(), Refusal> {
        let valid = match self {
            Self::Github { hostname, owner_id, repo_id } => {
                !hostname.is_empty() && hostname.len() <= NAME_MAX && hostname.split('.').all(name) && *owner_id > 0 && *repo_id > 0
            }
            Self::Native { repo_id } => name(repo_id),
        };
        if valid { Ok(()) } else { Err(Refusal::Invalid("repository identity")) }
    }
}

pub fn commit_valid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn reference_valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= PATH_MAX && !s.starts_with('-') && !s.ends_with('/') && !s.ends_with('.') && !s.contains("..") && !s.contains("@{")
        && s.bytes().all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b))
        && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock"))
}

pub fn path_valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= PATH_MAX && !s.starts_with('/') && !s.contains(['\\', ':']) && !s.chars().any(char::is_control)
        && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

impl Revision {
    pub fn validate(&self) -> Result<(), Refusal> {
        self.repository.validate()?;
        self.identity.validate()?;
        if !reference_valid(&self.requested) || !reference_valid(&self.reference) || !commit_valid(&self.commit) {
            return Err(Refusal::Invalid("revision"));
        }
        Ok(())
    }
}

pub fn image_valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= PATH_MAX && !s.contains("//") && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/@".contains(&b))
        && s.matches('@').count() <= 1 && s.split_once('@').is_none_or(|(_, d)| d.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())))
}

impl Manifest {
    pub fn validate(&self, reference: &str) -> Result<(), Refusal> {
        if self.bytes.len() > MANIFEST_MAX { return Err(Refusal::TooLarge { what: "manifest", limit: MANIFEST_MAX }); }
        if self.media_type.len() > NAME_MAX || self.media_type.is_empty() || self.media_type.chars().any(char::is_control) {
            return Err(Refusal::Invalid("manifest media type"));
        }
        let got = format!("sha256:{}", crate::sha256_hex(&self.bytes));
        if self.digest != got || reference.split_once('@').is_some_and(|(_, d)| d != got) { return Err(Refusal::Integrity("manifest")); }
        Ok(())
    }
}

impl Runtime {
    pub fn major(&self) -> Result<u32, Refusal> {
        let major = match self.using.as_str() {
            "node12" | "node16" | "node20" => 20,
            "node24" => 24,
            _ => return Err(Refusal::Unsupported("runtime")),
        };
        if !matches!(self.os.as_str(), "linux" | "macos") || !matches!(self.arch.as_str(), "amd64" | "arm64" | "arm") || (self.musl && (self.os != "linux" || self.arch == "arm")) {
            return Err(Refusal::Unsupported("runtime platform"));
        }
        if let Some(version) = &self.version {
            if version.len() > NAME_MAX { return Err(Refusal::Invalid("runtime version")); }
            let parts: Vec<_> = version.strip_prefix('v').unwrap_or_default().split('.').collect();
            if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u32>().is_err()) || parts.first().and_then(|p| p.parse::<u32>().ok()) != Some(major) {
                return Err(Refusal::Invalid("runtime version"));
            }
        }
        Ok(major)
    }
}

impl Archive {
    pub fn validate(&self, runtime: &Runtime) -> Result<(), Refusal> {
        let major = runtime.major()?;
        let parts: Vec<&str> = self.version.strip_prefix('v').unwrap_or_default().split('.').collect();
        let arch = match runtime.arch.as_str() { "amd64" => "x64", "arm64" => "arm64", _ => "armv7l" };
        let os = if runtime.os == "macos" { "darwin" } else { "linux" };
        let suffix = if runtime.musl && arch == "x64" { "-alpine-x64.tar.gz".to_owned() }
            else { format!("-{os}-{arch}{}.tar.gz", if runtime.musl { "-musl" } else { "" }) };
        let host = self.url.strip_prefix("https://").and_then(|s| s.split('/').next()).unwrap_or_default();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) || parts.first().and_then(|p| p.parse::<u32>().ok()) != Some(major)
            || !name(&self.file) || !self.file.starts_with(&format!("node-{}-", self.version)) || !self.file.ends_with(&suffix) || host.is_empty() || self.url.len() > PATH_MAX
            || self.url.contains(['@', '?', '#', '\\']) || self.url.chars().any(|c| c.is_control() || c.is_whitespace())
            || !self.url.ends_with(&format!("/{}", self.file)) || self.url.contains("/latest-")
            || self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) || runtime.version.as_ref().is_some_and(|v| v != &self.version) {
            return Err(Refusal::Invalid("runtime archive metadata"));
        }
        Ok(())
    }
}

/// A source repository's bounded graph. A parent absent from `commits` is unknown history,
/// not proof that an object is outside that history. Files can be larger than FILE_MAX in the
/// simulated upstream, but reads refuse before copying their bytes.
#[derive(Debug, Clone)]
pub struct SimRepository {
    pub identity: Identity,
    pub refs: BTreeMap<String, String>,
    pub default_branch: String,
    pub commits: BTreeMap<String, Vec<String>>,
    pub files: BTreeMap<(String, String), Vec<u8>>,
}

#[derive(Clone)]
struct Upstream {
    source: SimRepository,
    cached: Option<(Identity, BTreeMap<String, String>)>,
    online: bool,
    denied: bool,
}

#[derive(Default)]
struct State {
    repos: BTreeMap<Repository, Upstream>,
    manifests: BTreeMap<String, Manifest>,
    runtimes: BTreeMap<Runtime, Archive>,
    modes: BTreeMap<(Repository, String, String), u32>,
}

/// Deterministic, in memory, bounded. Mutations describe the outside world's changes, never
/// a decision by the simulator. Past a bound, insertion is refused atomically as Full.
#[derive(Clone, Default)]
pub struct SimDependencySources(Arc<Mutex<State>>);

impl SimDependencySources {
    pub fn put_repository(&self, repository: Repository, source: SimRepository) -> Result<(), Refusal> {
        repository.validate()?;
        source.identity.validate()?;
        if source.refs.len() > SIM_REFS { return Err(Refusal::Full { what: "refs", limit: SIM_REFS }); }
        if source.commits.len() > SIM_COMMITS { return Err(Refusal::Full { what: "commits", limit: SIM_COMMITS }); }
        if source.files.len() > SIM_FILES { return Err(Refusal::Full { what: "files", limit: SIM_FILES }); }
        if !reference_valid(&source.default_branch) || source.refs.iter().any(|(r, c)| !reference_valid(r) || !commit_valid(c))
            || source.commits.iter().any(|(c, p)| !commit_valid(c) || p.len() > 16 || p.iter().any(|c| !commit_valid(c)))
            || source.files.keys().any(|(c, p)| !commit_valid(c) || !path_valid(p)) { return Err(Refusal::Invalid("simulated repository")); }
        let key = repository.canonical();
        let mut state = self.0.lock();
        if !state.repos.contains_key(&key) && state.repos.len() >= SIM_REPOSITORIES { return Err(Refusal::Full { what: "repositories", limit: SIM_REPOSITORIES }); }
        let bytes = state.repos.iter().filter(|(r, _)| **r != key).flat_map(|(_, r)| r.source.files.values()).chain(source.files.values())
            .fold(0usize, |n, b| n.saturating_add(b.len()));
        if bytes > SIM_BYTES { return Err(Refusal::Full { what: "repository bytes", limit: SIM_BYTES }); }
        state.modes.retain(|(repo, _, _), _| repo != &key);
        state.repos.insert(key, Upstream { source, cached: None, online: true, denied: false });
        Ok(())
    }

    pub fn put_code_mode(&self, repository: &Repository, commit: &str, path: &str, mode: u32) -> Result<(), Refusal> {
        repository.validate()?;
        if !commit_valid(commit) || !path_valid(path) { return Err(Refusal::Invalid("code file")); }
        let mut state = self.0.lock();
        let repository = repository.canonical();
        let source = state.repos.get(&repository).ok_or(Refusal::Missing("repository"))?;
        let bytes = source.source.files.get(&(commit.into(), path.into())).ok_or(Refusal::Missing("file"))?;
        if !matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000)
            || mode == 0o160000 && !std::str::from_utf8(bytes).is_ok_and(commit_valid) { return Err(Refusal::Invalid("code file mode")); }
        let key = (repository, commit.into(), path.into());
        if !state.modes.contains_key(&key) && state.modes.len() >= SIM_FILES { return Err(Refusal::Full { what: "file modes", limit: SIM_FILES }); }
        state.modes.insert(key, mode);
        Ok(())
    }

    pub fn move_ref(&self, repository: &Repository, reference: &str, commit: &str) -> Result<(), Refusal> {
        if !reference_valid(reference) || !commit_valid(commit) { return Err(Refusal::Invalid("ref")); }
        let mut state = self.0.lock();
        let r = state.repos.get_mut(&repository.canonical()).ok_or(Refusal::Missing("repository"))?;
        if !r.source.commits.contains_key(commit) { return Err(Refusal::Missing("commit")); }
        if !r.source.refs.contains_key(reference) && r.source.refs.len() >= SIM_REFS { return Err(Refusal::Full { what: "refs", limit: SIM_REFS }); }
        r.source.refs.insert(reference.to_owned(), commit.to_owned());
        Ok(())
    }

    pub fn availability(&self, repository: &Repository, online: bool, denied: bool) -> Result<(), Refusal> {
        let mut state = self.0.lock();
        let r = state.repos.get_mut(&repository.canonical()).ok_or(Refusal::Missing("repository"))?;
        r.online = online;
        r.denied = denied;
        Ok(())
    }

    pub fn replace_identity(&self, repository: &Repository, identity: Identity) -> Result<(), Refusal> {
        identity.validate()?;
        let mut state = self.0.lock();
        state.repos.get_mut(&repository.canonical()).ok_or(Refusal::Missing("repository"))?.source.identity = identity;
        Ok(())
    }

    pub fn put_manifest(&self, reference: String, manifest: Manifest) -> Result<(), Refusal> {
        if !image_valid(&reference) { return Err(Refusal::Invalid("image reference")); }
        manifest.validate(&reference)?;
        let mut state = self.0.lock();
        if !state.manifests.contains_key(&reference) && state.manifests.len() >= SIM_METADATA { return Err(Refusal::Full { what: "manifests", limit: SIM_METADATA }); }
        let bytes = state.manifests.iter().filter(|(r, _)| **r != reference).map(|(_, m)| &m.bytes).chain(std::iter::once(&manifest.bytes)).fold(0usize, |n, b| n.saturating_add(b.len()));
        if bytes > SIM_BYTES { return Err(Refusal::Full { what: "manifest bytes", limit: SIM_BYTES }); }
        state.manifests.insert(reference, manifest);
        Ok(())
    }

    pub fn put_runtime(&self, runtime: Runtime, archive: Archive) -> Result<(), Refusal> {
        archive.validate(&runtime)?;
        let mut state = self.0.lock();
        if !state.runtimes.contains_key(&runtime) && state.runtimes.len() >= SIM_METADATA { return Err(Refusal::Full { what: "runtimes", limit: SIM_METADATA }); }
        state.runtimes.insert(runtime, archive);
        Ok(())
    }
}

fn checked<'a>(state: &'a State, revision: &Revision) -> Result<&'a SimRepository, Refusal> {
    revision.validate()?;
    let r = state.repos.get(&revision.repository.canonical()).ok_or(Refusal::Missing("repository"))?;
    if r.denied { return Err(Refusal::Denied); }
    let identity = if r.online { &r.source.identity } else { &r.cached.as_ref().ok_or(Refusal::Unavailable("repository identity"))?.0 };
    if *identity != revision.identity { return Err(Refusal::IdentityChanged); }
    if !r.source.commits.contains_key(&revision.commit) { return Err(Refusal::Missing("captured commit")); }
    Ok(&r.source)
}

fn contains(source: &SimRepository, head: &str, commit: &str) -> Result<bool, Refusal> {
    if !commit_valid(commit) { return Err(Refusal::Invalid("commit")); }
    if !source.commits.contains_key(commit) { return Err(Refusal::Missing("commit")); }
    let mut todo = vec![head.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(c) = todo.pop() {
        if c == commit { return Ok(true); }
        if !seen.insert(c.clone()) { continue; }
        todo.extend(source.commits.get(&c).ok_or(Refusal::Unavailable("commit history"))?.iter().cloned());
    }
    Ok(false)
}

impl DependencySources for SimDependencySources {
    fn compare(&self, before: Revision, after: Revision) -> BoxFuture<'_, Result<Difference, Refusal>> {
        Box::pin(async move {
            Difference { before: before.clone(), after: after.clone(), files: Vec::new() }.validate()?;
            let state = self.0.lock();
            let old = checked(&state, &before)?;
            let new = checked(&state, &after)?;
            if before.repository.canonical() != after.repository.canonical() { return Err(Refusal::Invalid("comparison repository")); }
            if before.identity != after.identity { return Err(Refusal::IdentityChanged); }
            let repository = before.repository.canonical();
            let paths: BTreeSet<_> = old.files.keys().filter(|(c, _)| c == &before.commit).chain(new.files.keys().filter(|(c, _)| c == &after.commit)).map(|(_, p)| p).collect();
            let mut files = Vec::new();
            let mut size = 0usize;
            for path in paths {
                let a = old.files.get(&(before.commit.clone(), path.clone()));
                let b = new.files.get(&(after.commit.clone(), path.clone()));
                let am = state.modes.get(&(repository.clone(), before.commit.clone(), path.clone())).copied().unwrap_or(0o100644);
                let bm = state.modes.get(&(repository.clone(), after.commit.clone(), path.clone())).copied().unwrap_or(0o100644);
                if a == b && am == bm { continue; }
                if files.len() >= DIFFERENCE_FILES { return Err(Refusal::Full { what: "changed files", limit: DIFFERENCE_FILES }); }
                size = size.saturating_add(path.len()).saturating_add(a.map_or(0, Vec::len)).saturating_add(b.map_or(0, Vec::len));
                if size > DIFFERENCE_MAX { return Err(Refusal::TooLarge { what: "code difference", limit: DIFFERENCE_MAX }); }
                files.push(CodeChange { path: path.clone(), before: a.map(|bytes| CodeFile { mode: am, bytes: bytes.clone() }), after: b.map(|bytes| CodeFile { mode: bm, bytes: bytes.clone() }) });
            }
            let difference = Difference { before, after, files };
            difference.validate()?;
            Ok(difference)
        })
    }

    fn resolve(&self, repository: Repository, reference: String) -> BoxFuture<'_, Result<Revision, Refusal>> {
        Box::pin(async move {
            repository.validate()?;
            if !reference_valid(&reference) { return Err(Refusal::Invalid("ref")); }
            let mut state = self.0.lock();
            let r = state.repos.get_mut(&repository.canonical()).ok_or(Refusal::Missing("repository"))?;
            if r.denied { return Err(Refusal::Denied); }
            if r.online { r.cached = Some((r.source.identity.clone(), r.source.refs.clone())); }
            let (identity, refs) = r.cached.as_ref().ok_or(Refusal::Unavailable("mirror"))?;
            let raw = commit_valid(&reference);
            let resolved = if raw { Vec::new() }
                else if reference == "HEAD" { vec![format!("refs/heads/{}", r.source.default_branch)] }
                else if reference.starts_with("refs/") { vec![reference.clone()] }
                else { vec![format!("refs/tags/{reference}"), format!("refs/heads/{reference}")] };
            let found = resolved.into_iter().find_map(|name| refs.get(&name).map(|c| (name, c.clone())));
            let (full, commit) = found.or_else(|| (raw && r.source.commits.contains_key(&reference.to_ascii_lowercase())).then(|| (reference.to_ascii_lowercase(), reference.to_ascii_lowercase())))
                .ok_or(if r.online { Refusal::Missing("ref") } else { Refusal::Unavailable("uncached ref") })?;
            Ok(Revision { repository: repository.canonical(), identity: identity.clone(), requested: reference, reference: full, commit, fresh: r.online })
        })
    }

    fn contains(&self, revision: Revision, commit: String) -> BoxFuture<'_, Result<bool, Refusal>> {
        Box::pin(async move { contains(checked(&self.0.lock(), &revision)?, &revision.commit, &commit) })
    }

    fn file(&self, revision: Revision, commit: String, path: String) -> BoxFuture<'_, Result<Vec<u8>, Refusal>> {
        Box::pin(async move {
            if !path_valid(&path) { return Err(Refusal::Invalid("file path")); }
            let state = self.0.lock();
            let source = checked(&state, &revision)?;
            if !contains(source, &revision.commit, &commit)? { return Err(Refusal::NotReachable); }
            let bytes = source.files.get(&(commit, path)).ok_or(Refusal::Missing("file"))?;
            if bytes.len() > FILE_MAX { return Err(Refusal::TooLarge { what: "file", limit: FILE_MAX }); }
            Ok(bytes.clone())
        })
    }

    fn manifest(&self, reference: String) -> BoxFuture<'_, Result<Manifest, Refusal>> {
        Box::pin(async move {
            if !image_valid(&reference) { return Err(Refusal::Invalid("image reference")); }
            self.0.lock().manifests.get(&reference).cloned().ok_or(Refusal::Missing("manifest"))
        })
    }

    fn runtime(&self, runtime: Runtime) -> BoxFuture<'_, Result<Archive, Refusal>> {
        Box::pin(async move {
            runtime.major()?;
            self.0.lock().runtimes.get(&runtime).cloned().ok_or(Refusal::Missing("runtime archive"))
        })
    }
}

pub mod contract;
pub use contract::{DependencySourcesFixture, FixtureChange, FixtureCommits, SimDependencySourcesFixture, cases};
