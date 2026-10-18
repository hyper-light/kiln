//! A repository's CI settings: `.kiln/config.yaml` on its default branch (docs/ci.md Appendix E).
//!
//! - `github_workflows`: `auto`, `on` or `off` (W02-R02), over the server's `[ci] github_workflows`.
//! - `default_token_permissions`: `read` or `write` (W10-R04), what `GITHUB_TOKEN` gets when a
//!   workflow and its job say nothing.
//! - `actions`: the repository's action policy (W07-R04): `allow`, `deny`, `require_sha_pins`,
//!   narrowing the server's.
//! - `untrusted`: how runs from forks go (W18).
//!
//! It's read from the default branch, never from the commit being run, so a change can't loosen
//! the rules it runs under.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kiln_ci::policy::Policy;
use kiln_ci::wf::GithubWorkflows;
use parking_lot::Mutex;
use serde::Deserialize;

use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    pub github_workflows: Option<GithubWorkflows>,
    pub default_token_permissions: Option<String>,
    pub actions: Option<Policy>,
    pub untrusted: Option<serde_yaml::Value>,
    pub github_api: Option<serde_yaml::Value>,
}

impl RepoConfig {
    /// The plan's default permissions: the repository's, else GitHub's restricted default.
    pub fn default_permissions(&self) -> kiln_ci::plan::perms::Default {
        match self.default_token_permissions.as_deref() {
            Some(p) if p.eq_ignore_ascii_case("write") => kiln_ci::plan::perms::Default::Write,
            _ => kiln_ci::plan::perms::Default::Read,
        }
    }
}

/// How long a repository's settings are read again at most.
const FRESH: Duration = Duration::from_secs(20);

static CACHE: Mutex<Option<HashMap<String, (Instant, RepoConfig)>>> = Mutex::new(None);

/// The first of `names` that trunk (the mirror's default branch) has: its name and contents.
pub async fn trunk_file(hub: &Hub, repo: &Repo, names: &[&str]) -> Option<(String, Vec<u8>)> {
    let mirror = gitops::mirror_path(hub, &repo.name).to_string_lossy().to_string();
    let branch = gitops::run_git(hub, &["-C", &mirror, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    for name in names {
        if let Ok(text) = gitops::run_git(hub, &["-C", &mirror, "show", &format!("{branch}:{name}")], LS_REMOTE_TIMEOUT).await {
            return Some(((*name).to_owned(), text));
        }
    }
    None
}

/// The repository's settings, from its mirror's default branch; none when there is no file or it
/// can't be read (which is logged).
pub async fn load(hub: &Hub, repo: &Repo) -> RepoConfig {
    if let Some((at, c)) = CACHE.lock().get_or_insert_with(HashMap::new).get(&repo.id)
        && at.elapsed() < FRESH
    {
        return c.clone();
    }
    let mut config = RepoConfig::default();
    if let Some((name, text)) = trunk_file(hub, repo, &[".kiln/config.yaml", ".kiln/config.yml"]).await {
        match serde_yaml::from_slice::<RepoConfig>(&text) {
            Ok(c) => config = c,
            Err(e) => tracing::warn!(repo = %repo.name, file = name, error = %e, "the repository's CI settings can't be read; using the server's"),
        }
    }
    CACHE.lock().get_or_insert_with(HashMap::new).insert(repo.id.clone(), (Instant::now(), config.clone()));
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appendix_e_settings_read_as_written() -> anyhow::Result<()> {
        let c: RepoConfig = serde_yaml::from_str("github_workflows: off\ndefault_token_permissions: write\nactions:\n  allow: ['actions/*']\n  require_sha_pins: true\nuntrusted: { secrets: none }\n")?;
        assert_eq!(c.github_workflows, Some(GithubWorkflows::Off));
        assert_eq!(c.default_permissions(), kiln_ci::plan::perms::Default::Write);
        assert_eq!(c.actions, Some(Policy { allow: vec!["actions/*".into()], deny: Vec::new(), require_sha_pins: true }));
        assert_eq!(RepoConfig::default().default_permissions(), kiln_ci::plan::perms::Default::Read);
        Ok(())
    }
}
