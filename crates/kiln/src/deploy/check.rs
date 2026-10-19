//! What a release is checked for before it is recorded (docs/deploy-api.md §9, step 1): that the
//! version it names is where its package comes from. A registry, chart repository or git remote
//! that says the version isn't there refuses the release; one kiln can't ask (unreachable, or
//! private to it) lets it through, and the provider has the last word.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::model::{Package, ReleaseFile};
use super::repo;
use crate::ci::integrations::{self, oci};
use crate::http::client::{Client, Method};
use crate::hub::Hub;

/// The largest chart repository index read.
const INDEX_LIMIT: usize = 64 << 20;

/// Why the release's version isn't there, when whoever serves its package says so.
pub async fn missing_version(hub: &Hub, package: &Package, file: &ReleaseFile) -> Option<String> {
    let version = file.version.as_deref()?;
    match package {
        Package::Helm { repo: chart_repo, chart, chart_version, .. } => {
            let version = match chart_version {
                Some(t) => repo::fill(t, file).ok()?,
                None => version.to_owned(),
            };
            match chart_repo.strip_prefix("oci://") {
                Some(base) => oci_chart(hub, base.trim_end_matches('/'), chart, &version).await,
                None => indexed_chart(chart_repo.trim_end_matches('/'), chart, &version).await,
            }
        }
        Package::Git { repo: git_repo, .. } => git_ref(hub, git_repo, version).await,
        // A platform's versions are its own: KilnCD finds out.
        Package::Platform { .. } => None,
    }
}

/// A chart in an OCI registry: its tag's manifest, with the registry's integration when there is
/// one. A tag that isn't there is only taken for missing when the repository answers its tags.
async fn oci_chart(hub: &Hub, base: &str, chart: &str, version: &str) -> Option<String> {
    // Helm keeps semver's `+` as `_` in a tag.
    let reference = oci::parse_reference(&format!("{base}/{chart}:{}", version.replace('+', "_"))).ok()?;
    let registry = match integrations::registry_for(hub, &reference.registry).await {
        Ok(Some(r)) => r,
        _ => oci::Registry::for_name(&reference.registry, None, false),
    };
    match oci::resolve(&registry, &reference).await {
        Ok(None) => match oci::tags(&registry, &reference.repository).await {
            Ok(tags) => Some(format!("{chart} {version} isn't in oci://{base}{}", newest(tags.iter().map(|t| t.replace('_', "+"))))),
            Err(e) => {
                tracing::debug!(chart, version, error = %format!("{e:#}"), "deploy: a chart's tags can't be listed; the provider checks it");
                None
            }
        },
        Ok(Some(_)) => None,
        Err(e) => {
            tracing::debug!(chart, version, error = %format!("{e:#}"), "deploy: a chart can't be looked up; the provider checks it");
            None
        }
    }
}

#[derive(Deserialize)]
struct Index {
    #[serde(default)]
    entries: BTreeMap<String, Vec<Entry>>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    version: String,
}

/// A chart in a Helm repository: its `index.yaml` lists the version.
async fn indexed_chart(base: &str, chart: &str, version: &str) -> Option<String> {
    let client = Client::public().ok()?;
    let resp = client.request(Method::Get, &format!("{base}/index.yaml")).timeout(Duration::from_secs(30)).send().await.ok()?;
    if !resp.is_success() {
        return None;
    }
    let bytes = resp.bytes(INDEX_LIMIT).await.ok()?;
    let index = crate::rt::blocking(move || serde_yaml::from_slice::<Index>(&bytes)).await.ok()?.ok()?;
    let Some(entries) = index.entries.get(chart) else { return Some(format!("{base} has no chart {chart}")) };
    if entries.iter().any(|e| e.version == version) {
        return None;
    }
    Some(format!("{chart} {version} isn't in {base}{}", newest(entries.iter().map(|e| e.version.clone()))))
}

/// A branch or tag of a git repository. A commit can't be asked for without fetching: it passes.
async fn git_ref(hub: &Hub, url: &str, version: &str) -> Option<String> {
    if version == "HEAD" || ((7..=40).contains(&version.len()) && version.bytes().all(|b| b.is_ascii_hexdigit())) {
        return None;
    }
    let refs = match crate::gitops::ls_remote(hub, url).await {
        Ok(refs) => refs,
        Err(e) => {
            tracing::debug!(url, version, error = %format!("{e:#}"), "deploy: a repository's refs can't be listed; the provider checks it");
            return None;
        }
    };
    let found = [format!("refs/heads/{version}"), format!("refs/tags/{version}"), version.to_owned()].iter().any(|r| refs.contains_key(r));
    if found { None } else { Some(format!("{url} has no branch or tag {version}")) }
}

/// The newest three versions, to say with a missing one.
fn newest(versions: impl Iterator<Item = String>) -> String {
    let mut known: Vec<((u64, u64, u64, bool), String)> = versions.filter_map(|v| semver(&v).map(|k| (k, v))).collect();
    known.sort_by_key(|k| std::cmp::Reverse(k.0));
    known.dedup_by(|a, b| a.1 == b.1);
    let shown: Vec<String> = known.into_iter().take(3).map(|(_, v)| v).collect();
    if shown.is_empty() { String::new() } else { format!(" (the newest: {})", shown.join(", ")) }
}

/// A version's order: major, minor, patch, and whether it is a release rather than a pre-release.
fn semver(v: &str) -> Option<(u64, u64, u64, bool)> {
    let v = v.strip_prefix('v').unwrap_or(v);
    let core = v.split('+').next().unwrap_or(v);
    let (core, pre) = match core.split_once('-') {
        Some((c, _)) => (c, true),
        None => (core, false),
    };
    let mut parts = core.split('.').map(str::parse::<u64>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch, !pre))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_versions_are_said_newest_first() {
        let tags = ["6.10.1", "6.9.0", "6.11.0", "6.11.0-rc.1", "sha256-abc.sig", "6.10.2"].map(str::to_owned);
        assert_eq!(newest(tags.into_iter()), " (the newest: 6.11.0, 6.11.0-rc.1, 6.10.2)");
        assert_eq!(newest(std::iter::empty()), "");
        assert_eq!(semver("v1.2"), Some((1, 2, 0, true)));
        assert_eq!(semver("latest"), None);
    }
}
