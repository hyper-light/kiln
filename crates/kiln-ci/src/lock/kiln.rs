//! kiln's supplemental lock (W29-R24): sources GitHub's file cannot name, reusable workflows,
//! image digests and platform-specific runtime archives. Reading and writing use only supplied
//! bytes; source access, clocks and accepting a moved version belong outside this module.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::wf::diag::{Diag, Pos, Range};
use crate::wf::yaml::{self, Node, Scalar};

pub const PATH: &str = ".kiln/lock";
pub const VERSION: u32 = 1;
pub const HEADER: &str = "# Written by kiln (docs/ci.md W29-R24). Don't edit it: `ci.locks.update` writes it.\n";
pub const MAX_ITEMS: usize = 4096;
const MAX_NAME: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Identity {
    Github { hostname: String, owner_id: i64, repo_id: i64 },
    Kiln { id: String },
}

/// A Git source at an immutable commit. `repo` is github:<owner_id>/<repo_id> or kiln:<id>.
/// Full refs retain branch/tag identity even when the name looks like a version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Git {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub commit: String,
    pub repo: String,
    pub hostname: Option<String>,
    pub uses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image { pub digest: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    pub version: String,
    pub url: String,
    pub file: String,
    pub sha256: String,
}

/// A shorthand SHA-256 (the R24 example), or exact archives by os/arch[/musl]. New generation
/// writes archives with their immutable locations, so an older accepted build can be fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Runtime {
    pub sha256: Option<String>,
    pub archives: BTreeMap<String, Archive>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dependency { Git(Git), Image(Image), Runtime(Runtime) }

impl Dependency {
    pub fn uses(&self) -> &[String] { match self { Self::Git(g) => &g.uses, _ => &[] } }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct File {
    pub version: u32,
    pub workflows: BTreeMap<String, Vec<String>>,
    pub dependencies: BTreeMap<String, Dependency>,
}

impl Default for File {
    fn default() -> Self { Self { version: VERSION, workflows: BTreeMap::new(), dependencies: BTreeMap::new() } }
}

type Read<T> = Result<T, Box<Diag>>;

fn error(node: Option<&Node>, message: impl Into<String>) -> Box<Diag> {
    let at = Pos { line: 1, col: 1, offset: 0 };
    Box::new(Diag::error("KCI1022", message, node.map_or(Range { start: at, end: at }, |n| n.range)).at(PATH))
}

fn string(node: &Node) -> Read<String> {
    match node.as_scalar() { Some(Scalar::Str(s)) => Ok(s.clone()), _ => Err(error(Some(node), "expected a lock string (quote a numeric ref)")) }
}

fn map(node: &Node) -> Read<&[(Node, Node)]> { node.as_map().ok_or_else(|| error(Some(node), "expected a lock mapping")) }
fn required<'a>(node: &'a Node, name: &str) -> Read<&'a Node> { node.get(name).ok_or_else(|| error(Some(node), format!("missing required lock field {name:?}"))) }
fn field(node: &Node, name: &str) -> Read<String> { string(required(node, name)?) }

fn fields(node: &Node, allowed: &[&str]) -> Read<()> {
    for (key, _) in map(node)? {
        if !matches!(key.as_scalar(), Some(Scalar::Str(s)) if allowed.contains(&s.as_str())) { return Err(error(Some(key), "unknown supplemental lock field")); }
    }
    Ok(())
}

fn literal(s: &str) -> bool { !s.is_empty() && s.len() <= MAX_NAME && !s.chars().any(|c| c.is_control() || c.is_whitespace()) && !s.contains("${{") }
fn component(s: &str) -> bool { !s.is_empty() && s.len() <= 256 && s.bytes().any(|b| b != b'.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) }
fn path(s: &str) -> bool { literal(s) && !s.starts_with('/') && !s.contains(['\\', ':']) && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..") }
fn hex(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) }

pub fn reference(s: &str) -> bool {
    literal(s) && s != "@" && !s.starts_with(['-', '/']) && !s.ends_with(['/', '.']) && !s.contains("..") && !s.contains("@{")
        && !s.bytes().any(|b| b"~^:?*[\\".contains(&b)) && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock"))
}

pub fn workflow_key(key: &str) -> bool {
    let name = key.split_once('@').map_or(key, |(name, _)| name).strip_prefix("kiln://").unwrap_or_else(|| key.split_once('@').map_or(key, |(name, _)| name));
    let below = name.splitn(3, '/').nth(2).unwrap_or_default();
    (below.starts_with(".github/workflows/") && (below.ends_with(".yml") || below.ends_with(".yaml")))
        || (below.starts_with(".kiln/") && [".yml", ".yaml", ".ts", ".js"].iter().any(|s| below.ends_with(s)))
}

/// Source spelling is kept; only repository owner/name and immutable digest hex normalize.
pub fn canonical_key(key: &str) -> Option<String> {
    if !literal(key) { return None; }
    if key.starts_with("docker://") || key.starts_with("image://") || key.starts_with("microvm://") || key.starts_with("executor://") || key.starts_with("runtime://") {
        return Some(key.split_once("@sha256:").map_or_else(|| key.into(), |(name, hash)| format!("{name}@sha256:{}", hash.to_ascii_lowercase())));
    }
    if key.strip_prefix("node").is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) { return Some(key.into()); }
    let (name, git_ref) = key.split_once('@')?;
    if !reference(git_ref) { return None; }
    let (native, name) = name.strip_prefix("kiln://").map_or((false, name), |n| (true, n));
    let mut parts = name.splitn(3, '/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if !component(owner) || !component(repo) { return None; }
    let below = parts.next();
    if below.is_some_and(|p| !path(p)) { return None; }
    let git_ref = if matches!(git_ref.len(), 40 | 64) && git_ref.bytes().all(|b| b.is_ascii_hexdigit()) { git_ref.to_ascii_lowercase() } else { git_ref.into() };
    Some(format!("{}{}/{}{}@{git_ref}", if native { "kiln://" } else { "" }, owner.to_ascii_lowercase(), repo.to_ascii_lowercase(), below.map(|p| format!("/{p}")).unwrap_or_default()))
}

impl Git {
    pub fn identity(&self, home: &str) -> Option<Identity> {
        if let Some(id) = self.repo.strip_prefix("kiln:") { return (component(id) && self.hostname.is_none()).then(|| Identity::Kiln { id: id.into() }); }
        let (owner, repo) = self.repo.strip_prefix("github:")?.split_once('/')?;
        let decimal = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<i64>().ok()).flatten().filter(|n| *n > 0);
        let hostname = self.hostname.as_deref().unwrap_or(home);
        let (owner_id, repo_id) = (decimal(owner)?, decimal(repo)?);
        super::github::hostname(hostname).then(|| Identity::Github { hostname: hostname.into(), owner_id, repo_id })
    }
}

fn full_sha(s: &str) -> bool { matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) }

fn image_key(key: &str) -> Option<&str> {
    ["docker://", "image://", "microvm://", "executor://"].iter().find_map(|p| key.strip_prefix(p))
}

fn image_reference(s: &str) -> bool {
    !s.is_empty() && !s.contains("//") && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/@".contains(&b))
        && s.matches('@').count() <= 1 && s.split_once('@').is_none_or(|(_, d)| d.strip_prefix("sha256:").is_some_and(hex))
        && !s.starts_with(['/', ':', '@']) && !s.ends_with(['/', ':', '@']) && s.split('/').all(|p| p != "." && p != ".." && !p.is_empty())
}

fn runtime_major(key: &str) -> Option<u32> {
    match key.strip_prefix("runtime://").unwrap_or(key) { "node12" | "node16" | "node20" => Some(20), "node24" => Some(24), _ => None }
}

impl Archive {
    /// Platform names are Go's os/arch, with an optional musl suffix. Match the same
    /// immutable Node archive naming used by the source seam, without downloading anything.
    pub fn valid_for(&self, runtime: &str, platform: &str) -> bool {
        if !literal(&self.version) || !literal(&self.url) || !component(&self.file) || !hex(&self.sha256) { return false; }
        let mut parts = platform.split('/');
        let (Some(os), Some(arch)) = (parts.next(), parts.next()) else { return false };
        let musl = match parts.next() { None => false, Some("musl") => true, _ => return false };
        if parts.next().is_some() || !matches!(os, "linux" | "macos") || !matches!(arch, "amd64" | "arm64" | "arm") || (musl && (os != "linux" || arch == "arm")) { return false; }
        let version: Vec<_> = self.version.strip_prefix('v').unwrap_or_default().split('.').collect();
        let os = if os == "macos" { "darwin" } else { os };
        let arch = match arch { "amd64" => "x64", "arm64" => "arm64", _ => "armv7l" };
        let suffix = if musl && arch == "x64" { "-alpine-x64.tar.gz".into() } else { format!("-{os}-{arch}{}.tar.gz", if musl { "-musl" } else { "" }) };
        let host = self.url.strip_prefix("https://").and_then(|s| s.split('/').next()).unwrap_or_default();
        runtime_major(runtime).is_some_and(|major| version.len() == 3 && version.first().and_then(|n| n.parse::<u32>().ok()) == Some(major))
            && version.iter().all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            && self.file.starts_with(&format!("node-{}-", self.version)) && self.file.ends_with(&suffix)
            && !host.is_empty() && !self.url.contains(['@', '?', '#', '\\']) && !self.url.contains("/latest-")
            && self.url.ends_with(&format!("/{}", self.file))
    }
}

impl Dependency {
    pub fn validate(&self, key: &str, home: &str) -> Result<(), String> {
        let Some(canonical) = canonical_key(key) else { return Err("invalid literal dependency key".into()) };
        if canonical != key { return Err("dependency key must use canonical repository names and commit hex".into()); }
        match self {
            Self::Git(g) => {
                if image_key(key).is_some() || runtime_major(key).is_some() || key.starts_with("runtime://") { return Err("a git pin needs owner/repository[/path]@ref".into()); }
                let (Some((_, requested)), Some(commit)) = (key.split_once('@'), super::github::digest(&g.commit)) else { return Err("invalid git commit digest".into()) };
                let bound = if full_sha(requested) { requested.eq_ignore_ascii_case(commit) }
                    else if requested == "HEAD" { g.git_ref.starts_with("refs/heads/") }
                    else { g.git_ref == requested || ["refs/heads/", "refs/tags/"].iter().any(|p| g.git_ref.strip_prefix(p) == Some(requested)) };
                let recorded = ["refs/heads/", "refs/tags/"].iter().any(|p| g.git_ref.strip_prefix(p).is_some_and(|r| !r.is_empty()));
                if !reference(&g.git_ref) || !recorded || !bound { return Err("recorded git ref must name the requested branch, tag or commit".into()); }
                if g.identity(home).is_none() || (key.starts_with("kiln://") && !matches!(g.identity(home), Some(Identity::Kiln { .. }))) { return Err("invalid repository identity for this git source".into()); }
                if g.uses.iter().any(|u| canonical_key(u).as_deref() != Some(u)) { return Err("invalid transitive dependency key".into()); }
            }
            Self::Image(i) => {
                let Some(reference) = image_key(key).filter(|r| image_reference(r)) else { return Err("an image pin needs a literal image reference".into()) };
                if !i.digest.strip_prefix("sha256:").is_some_and(hex) || reference.split_once('@').is_some_and(|(_, d)| !d.eq_ignore_ascii_case(&i.digest)) { return Err("invalid or contradictory image digest".into()); }
            }
            Self::Runtime(r) => {
                if runtime_major(key).is_none() || (r.sha256.is_none() && r.archives.is_empty()) || r.sha256.as_ref().is_some_and(|s| !hex(s)) { return Err("a runtime needs Node's SHA-256 or immutable platform archives".into()); }
                if r.archives.iter().any(|(p, a)| !a.valid_for(key, p)) { return Err("runtime archive does not match its version, platform or immutable URL".into()); }
                if r.sha256.is_some() && !r.archives.is_empty() { return Err("use platform archives or an unqualified runtime checksum, not both".into()); }
            }
        }
        Ok(())
    }
}

fn pins(node: &Node) -> Read<Vec<String>> {
    let items = node.as_seq().ok_or_else(|| error(Some(node), "expected a list of dependency keys"))?;
    let mut pins = BTreeSet::new();
    for item in items {
        let pin = canonical_key(&string(item)?).ok_or_else(|| error(Some(item), "a lock key must be a literal source reference"))?;
        pins.insert(pin);
    }
    Ok(pins.into_iter().collect())
}

fn dependency(key: &str, node: &Node) -> Read<Dependency> {
    let value = if node.get("ref").is_some() || node.get("repo").is_some() || node.get("commit").is_some() {
        fields(node, &["ref", "commit", "repo", "hostname", "uses"])?;
        Dependency::Git(Git { git_ref: field(node, "ref")?, commit: field(node, "commit")?.to_ascii_lowercase(), repo: field(node, "repo")?, hostname: node.get("hostname").map(string).transpose()?, uses: node.get("uses").map(pins).transpose()?.unwrap_or_default() })
    } else if node.get("digest").is_some() {
        fields(node, &["digest"])?;
        Dependency::Image(Image { digest: field(node, "digest")?.to_ascii_lowercase() })
    } else {
        fields(node, &["sha256", "archives"])?;
        let mut archives = BTreeMap::new();
        if let Some(items) = node.get("archives") {
            for (p, a) in map(items)? {
                fields(a, &["version", "url", "file", "sha256"])?;
                archives.insert(string(p)?, Archive { version: field(a, "version")?, url: field(a, "url")?, file: field(a, "file")?, sha256: field(a, "sha256")?.to_ascii_lowercase() });
            }
        }
        Dependency::Runtime(Runtime { sha256: node.get("sha256").map(string).transpose()?.map(|s| s.to_ascii_lowercase()), archives })
    };
    value.validate(key, "github.com").map_err(|m| error(Some(node), m))?;
    Ok(value)
}

fn read(root: &Node) -> Read<File> {
    fields(root, &["version", "workflows", "dependencies"])?;
    if !matches!(required(root, "version")?.as_scalar(), Some(Scalar::Int(1))) { return Err(error(root.get("version"), "unsupported kiln lock version; expected version: 1")); }
    let mut file = File::default();
    if let Some(workflows) = root.get("workflows") {
        for (k, v) in map(workflows)? {
            let workflow = string(k)?;
            if !path(&workflow) { return Err(error(Some(k), "a workflow needs a safe repository-relative path")); }
            file.workflows.insert(workflow, pins(v)?);
        }
    }
    if let Some(dependencies) = root.get("dependencies") {
        for (k, v) in map(dependencies)? {
            let key = canonical_key(&string(k)?).ok_or_else(|| error(Some(k), "invalid literal dependency key"))?;
            let value = dependency(&key, v)?;
            if file.dependencies.get(&key).is_some_and(|old| old != &value) { return Err(error(Some(k), format!("conflicting metadata for {key:?}"))); }
            file.dependencies.insert(key, value);
        }
    }
    file.bounds().map_err(|m| error(Some(root), m))?;
    if let Some(key) = file.cycle() { return Err(error(Some(root), format!("dependency graph has a cycle at {key:?}"))); }
    Ok(file)
}

pub fn parse(source: &str) -> Result<File, Vec<Diag>> {
    let mut diagnostics = Vec::new();
    let root = yaml::parse(source, &mut diagnostics);
    if diagnostics.iter().any(Diag::is_error) {
        return Err(diagnostics.into_iter().filter(Diag::is_error).map(|mut d| {
            d.code = "KCI1022"; d.path = PATH.into();
            d.range.start.line = d.range.start.line.max(1); d.range.start.col = d.range.start.col.max(1);
            d.range.end.line = d.range.end.line.max(1); d.range.end.col = d.range.end.col.max(1);
            d
        }).collect());
    }
    root.ok_or_else(|| vec![*error(None, "a kiln lock needs a version and a mapping")]).and_then(|n| read(&n).map_err(|d| vec![*d]))
}

impl File {
    fn bounds(&self) -> Result<(), String> {
        let items = self.workflows.len().saturating_add(self.dependencies.len()).saturating_add(self.workflows.values().map(Vec::len).sum::<usize>())
            .saturating_add(self.dependencies.values().map(|d| d.uses().len() + match d { Dependency::Runtime(r) => r.archives.len(), _ => 0 }).sum::<usize>());
        if items > MAX_ITEMS { return Err(format!("a kiln lock holds at most {MAX_ITEMS} entries and dependency edges")); }
        Ok(())
    }

    fn cycle(&self) -> Option<String> {
        let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
        for root in self.dependencies.keys() {
            let mut todo = vec![(root.as_str(), false)];
            while let Some((key, leaving)) = todo.pop() {
                if leaving { active.remove(key); done.insert(key); continue; }
                if done.contains(key) { continue; }
                if !active.insert(key) { return Some(key.into()); }
                todo.push((key, true));
                if let Some(d) = self.dependencies.get(key) { todo.extend(d.uses().iter().rev().map(|u| (u.as_str(), false))); }
            }
        }
        None
    }

    /// Includes unknown keys: the verifier reports missing entries instead of overlooking them.
    pub fn closure(&self, workflow: &str) -> Option<Vec<String>> {
        let mut todo = self.workflows.get(workflow)?.clone();
        let mut seen = BTreeSet::new();
        while let Some(key) = todo.pop() {
            if seen.insert(key.clone()) && let Some(d) = self.dependencies.get(&key) { todo.extend(d.uses().iter().cloned()); }
        }
        Some(seen.into_iter().collect())
    }

    pub fn write(&self) -> Result<String, Vec<Diag>> {
        self.bounds().map_err(|m| vec![*error(None, m)])?;
        if self.version != VERSION { return Err(vec![*error(None, "unsupported kiln lock version")]); }
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let list = |out: &mut String, values: &[String], indent: &str| {
            for value in values.iter().collect::<BTreeSet<_>>() { out.push_str(&format!("{indent}- {}\n", quote(value))); }
        };
        let mut out = format!("{HEADER}version: 1\n");
        if !self.workflows.is_empty() { out.push_str("workflows:\n"); }
        for (workflow, uses) in &self.workflows {
            if !path(workflow) { return Err(vec![*error(None, "invalid workflow path")]); }
            out.push_str(&format!("  {}:{}", quote(workflow), if uses.is_empty() { " []\n" } else { "\n" }));
            list(&mut out, uses, "    ");
            if out.len() > yaml::MAX_FILE_BYTES { return Err(vec![*error(None, "kiln lock exceeds 1 MiB")]); }
        }
        if !self.dependencies.is_empty() { out.push_str("dependencies:\n"); }
        for (key, value) in &self.dependencies {
            value.validate(key, "github.com").map_err(|m| vec![*error(None, format!("{key}: {m}"))])?;
            out.push_str(&format!("  {}:\n", quote(key)));
            match value {
                Dependency::Git(g) => {
                    out.push_str(&format!("    ref: {}\n    commit: {}\n    repo: {}\n", quote(&g.git_ref), quote(&g.commit.to_ascii_lowercase()), quote(&g.repo)));
                    if let Some(hostname) = &g.hostname { out.push_str(&format!("    hostname: {}\n", quote(hostname))); }
                    if !g.uses.is_empty() { out.push_str("    uses:\n"); list(&mut out, &g.uses, "      "); }
                }
                Dependency::Image(i) => out.push_str(&format!("    digest: {}\n", quote(&i.digest.to_ascii_lowercase()))),
                Dependency::Runtime(r) => {
                    if let Some(sha) = &r.sha256 { out.push_str(&format!("    sha256: {}\n", quote(&sha.to_ascii_lowercase()))); }
                    if !r.archives.is_empty() { out.push_str("    archives:\n"); }
                    for (p, a) in &r.archives {
                        out.push_str(&format!("      {}:\n        version: {}\n        url: {}\n        file: {}\n        sha256: {}\n", quote(p), quote(&a.version), quote(&a.url), quote(&a.file), quote(&a.sha256.to_ascii_lowercase())));
                    }
                }
            }
            if out.len() > yaml::MAX_FILE_BYTES { return Err(vec![*error(None, "kiln lock exceeds 1 MiB")]); }
        }
        parse(&out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
