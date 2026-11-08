//! Immutable non-Git inputs carried to a leg (W29-R24). Plain JSON, shared by preflight,
//! saved run recovery and the runner; source access and downloading belong to their adapters.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::kiln::{Archive, Dependency, Image};

pub const VERSION: u32 = 1;
pub const MAX_ITEMS: usize = 4096;
pub const MAX_BYTES: usize = 8 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dockerfile {
    /// Relative to the action's directory. Its build context stays in the source tree.
    pub path: String,
    pub source_sha256: String,
    /// The separately materialized Dockerfile, with its image references replaced by digests.
    pub locked: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Locks {
    pub version: u32,
    /// Original OCI reference (without docker://) to its full immutable name@sha256:digest.
    pub images: BTreeMap<String, String>,
    /// Runtime name to os/arch[/musl] archives, each at an exact version and SHA-256.
    pub runtimes: BTreeMap<String, BTreeMap<String, Archive>>,
    /// By the action's original uses, including local actions.
    pub dockerfiles: BTreeMap<String, Dockerfile>,
}

impl Default for Locks {
    fn default() -> Self { Self { version: VERSION, images: BTreeMap::new(), runtimes: BTreeMap::new(), dockerfiles: BTreeMap::new() } }
}

fn hex(s: &str) -> bool { s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) }
fn literal(s: &str) -> bool { !s.is_empty() && s.len() <= 4096 && !s.contains("${{") && !s.chars().any(|c| c.is_control() || c.is_whitespace()) }
fn relative(s: &str) -> bool { literal(s) && !s.starts_with('/') && !s.contains(['\\', ':']) && s.split('/').all(|p| !p.is_empty() && p != "." && p != "..") }

/// Keep registry ports while removing only the image tag. Digest input must agree with the
/// requested immutable reference; no expression or credential URL becomes a guessed pin.
pub fn pin_image(reference: &str, digest: &str) -> Result<String, String> {
    let reference = reference.strip_prefix("docker://").unwrap_or(reference);
    let key = super::kiln::canonical_key(&format!("docker://{reference}")).ok_or("invalid literal image reference")?;
    Dependency::Image(Image { digest: digest.to_ascii_lowercase() }).validate(&key, "github.com")?;
    let name = reference.split_once('@').map_or(reference, |(name, _)| name);
    let name = name.rsplit_once(':').filter(|(_, suffix)| !suffix.contains('/')).map_or(name, |(name, _)| name);
    Ok(format!("{name}@{}", digest.to_ascii_lowercase()))
}

impl Locks {
    pub fn is_empty(&self) -> bool { self.images.is_empty() && self.runtimes.is_empty() && self.dockerfiles.is_empty() }

    pub fn image(&self, reference: &str) -> Result<String, String> {
        let reference = reference.strip_prefix("docker://").unwrap_or(reference);
        self.images.get(reference).cloned().or_else(|| self.images.values().find(|pin| pin.as_str() == reference).cloned())
            .ok_or_else(|| format!("KCI4050 {reference}: image is missing from this leg's dependency snapshot"))
    }

    pub fn runtime(&self, using: &str, platform: &str) -> Result<&Archive, String> {
        let canonical = match using { "node12" | "node16" => "node20", _ => using };
        self.runtimes.get(using).or_else(|| self.runtimes.get(canonical)).and_then(|a| a.get(platform))
            .ok_or_else(|| format!("KCI4050 {using}: no locked archive for {platform}"))
    }

    /// Each runtime must have an archive for the runner's OS and architecture. The runner
    /// additionally checks the job container's C library before downloading anything.
    pub fn runtime_requirements(&self) -> Vec<String> {
        self.runtimes.values().map(|archives| {
            let platforms = archives.keys().filter_map(|platform| {
                let mut parts = platform.split('/');
                let (os, arch) = (parts.next()?, parts.next()?);
                let arch = match arch { "amd64" => "x86_64", "arm64" => "aarch64", other => other };
                Some(format!("(os == {os} && arch == {arch})"))
            }).collect::<std::collections::BTreeSet<_>>();
            format!("({})", platforms.into_iter().collect::<Vec<_>>().join(" || "))
        }).collect()
    }

    /// W29-R24: called before any executor starts. Reapplying an accepted assignment is
    /// idempotent; an unlisted pool image or expression is never left mutable.
    pub fn assignment(&self, assignment: &mut crate::wire::AssignmentV2) -> Result<(), String> {
        self.validate()?;
        let mut updated = assignment.clone();
        if let Some(container) = &mut updated.job_container {
            let image = self.image(updated.container.as_deref().unwrap_or(&container.image.src))?;
            container.image.src = image.clone(); updated.container = Some(image);
        } else if let Some(image) = &mut updated.container { *image = self.image(image)?; }
        for (name, container) in &mut updated.job_services {
            let image = self.image(updated.services.get(name).map_or(container.image.src.as_str(), String::as_str))?;
            container.image.src = image.clone(); updated.services.insert(name.clone(), image);
        }
        for image in updated.services.values_mut() { *image = self.image(image)?; }
        // A job container replaces the Docker executor's default image. A microVM still
        // boots its own image before starting that job container inside the guest.
        if updated.executor.kind == "docker" && updated.job_container.is_some() { updated.executor.image = None; }
        else if let Some(image) = &mut updated.executor.image { *image = self.image(image)?; }
        else if updated.executor.kind == "firecracker" || updated.executor.kind == "docker" && updated.job_container.is_none() {
            return Err("KCI4050 the locked job has no captured executor image".into());
        }
        *assignment = updated;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION { return Err("KCI1022 unsupported execution lock version".into()); }
        let count = self.images.len().saturating_add(self.dockerfiles.len()).saturating_add(self.runtimes.len())
            .saturating_add(self.runtimes.values().map(BTreeMap::len).sum::<usize>());
        if count > MAX_ITEMS { return Err("KCI1022 execution lock exceeds 4096 entries".into()); }
        for (reference, pinned) in &self.images {
            let digest = pinned.split_once('@').map(|(_, d)| d).ok_or("KCI1022 a locked image needs its digest")?;
            if pin_image(reference, digest).map_err(|e| format!("KCI1022 {e}"))? != *pinned { return Err("KCI1022 image snapshot does not match its source reference".into()); }
        }
        for (using, archives) in &self.runtimes {
            if archives.is_empty() || archives.iter().any(|(platform, a)| !a.valid_for(using, platform)) { return Err("KCI1022 invalid platform runtime archive".into()); }
        }
        for (uses, dockerfile) in &self.dockerfiles {
            if !literal(uses) || !relative(&dockerfile.path) || !hex(&dockerfile.source_sha256) || dockerfile.locked.is_empty() || dockerfile.locked.len() > 1 << 20 {
                return Err("KCI1022 invalid locked Dockerfile".into());
            }
        }
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(bytes.len());
                if self.0 > MAX_BYTES { return Err(std::io::Error::other("execution lock exceeds 8 MiB")); }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        serde_json::to_writer(Count(0), self).map_err(|_| "KCI1022 execution lock cannot be encoded within 8 MiB".into())
    }

    /// Merge inputs needed by a job and its composite actions. A conflicting original ref
    /// never picks whichever source happened to be visited first.
    pub fn merge(&mut self, other: &Self) -> Result<(), String> {
        self.validate()?;
        other.validate()?;
        let mut merged = self.clone();
        for (key, value) in &other.images {
            if merged.images.get(key).is_some_and(|old| old != value) { return Err(format!("KCI4054 {key}: image snapshots name both {:?} and {value}", merged.images.get(key))); }
            merged.images.insert(key.clone(), value.clone());
        }
        for (using, archives) in &other.runtimes {
            let into = merged.runtimes.entry(using.clone()).or_default();
            for (platform, archive) in archives {
                if into.get(platform).is_some_and(|old| old != archive) { return Err(format!("KCI4054 {using}/{platform}: runtime snapshots name both {:?} and {archive:?}", into.get(platform))); }
                into.insert(platform.clone(), archive.clone());
            }
        }
        for (uses, dockerfile) in &other.dockerfiles {
            if merged.dockerfiles.get(uses).is_some_and(|old| old != dockerfile) { return Err(format!("KCI4054 {uses}: Dockerfile snapshots disagree")); }
            merged.dockerfiles.insert(uses.clone(), dockerfile.clone());
        }
        merged.validate()?;
        *self = merged;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
