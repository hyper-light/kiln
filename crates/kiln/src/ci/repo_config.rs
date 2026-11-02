//! A repository's CI settings: `.kiln/config.yaml` on its default branch (docs/ci.md Appendix E).
//!
//! - `github_workflows`: `auto`, `on` or `off` (W02-R02), over the server's `[ci] github_workflows`.
//! - `default_token_permissions`: `read` or `write` (W10-R04), what `GITHUB_TOKEN` gets when a
//!   workflow and its job say nothing.
//! - `actions`: the repository's action policy (W07-R04): `allow`, `deny`, `require_sha_pins`,
//!   narrowing the server's.
//! - `untrusted`: how runs from forks go (W18).
//! - `share`: who may use its actions and reusable workflows (W29-R17): `none` (the default),
//!   `owner` or `everyone`.
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
    pub share: Option<String>,
    /// W29-R22: `updates: { actions: on, cooldown: 3d, group: weekly }`, kiln's update drafts for
    /// what its workflows use.
    pub updates: Option<UpdatesConfig>,
    /// W29-R26: only the trusted default branch supplies these settings.
    pub require_lock: Option<kiln_ci::lock::verify::Require>,
    pub moved_tags: Option<kiln_ci::lock::verify::MovedTags>,
}

/// `.kiln/config.yaml`'s `updates:` as written (W29-R22).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    pub actions: Option<serde_yaml::Value>,
    pub cooldown: Option<serde_yaml::Value>,
    pub group: Option<String>,
}

/// A scalar as YAML wrote it: `on`, `true`, `0`, `3d`.
fn scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl RepoConfig {
    /// Defaults stay off/flag. `Policy::stricter` also makes enforce imply moved-tag refusal.
    pub fn locks(&self) -> kiln_ci::lock::verify::Policy {
        kiln_ci::lock::verify::Policy { require: self.require_lock.unwrap_or_default(), moved_tags: self.moved_tags.unwrap_or_default() }
    }

    /// Its update drafts (W29-R22): off unless `updates.actions` says on.
    pub fn updates(&self) -> kiln_ci::updates::Updates {
        let u = self.updates.clone().unwrap_or_default();
        let (actions, cooldown) = (u.actions.as_ref().and_then(scalar), u.cooldown.as_ref().and_then(scalar));
        kiln_ci::updates::of_config(actions.as_deref(), cooldown.as_deref(), u.group.as_deref())
    }

    /// Who may use its actions and reusable workflows (W29-R17): not shared unless it says.
    pub fn share(&self) -> kiln_ci::sharing::Access {
        self.share.as_deref().and_then(kiln_ci::sharing::Access::of_share).unwrap_or_default()
    }

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

/// The first of `names` that trunk (the default branch of the repository kiln hosts, else of its
/// mirror) has: its name and contents.
pub async fn trunk_file(hub: &Hub, repo: &Repo, names: &[&str]) -> Option<(String, Vec<u8>)> {
    // A repository kiln hosts: its own git, which has a push before the mirror does.
    let mirror = super::hosting::native_path(hub, repo).unwrap_or_else(|| gitops::mirror_path(hub, &repo.name)).to_string_lossy().to_string();
    let branch = gitops::run_git(hub, &["-C", &mirror, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    for name in names {
        if let Ok(text) = gitops::run_git(hub, &["-C", &mirror, "show", &format!("{branch}:{name}")], LS_REMOTE_TIMEOUT).await {
            return Some(((*name).to_owned(), text));
        }
    }
    None
}

/// W29-R26: a feature's config cannot relax its own checks. Read the trusted branch at one
/// captured commit, bounded, and distinguish absence from a file that cannot be verified.
pub async fn lock_policy(hub: &Hub, repo: &Repo) -> Result<kiln_ci::lock::verify::Policy, String> {
    let dir = super::hosting::native_path(hub, repo).unwrap_or_else(|| gitops::mirror_path(hub, &repo.name));
    if !dir.join("HEAD").is_file() { return Err("KCI4055 trusted lock policy is unavailable: the repository has no default-branch mirror".into()); }
    let branch = match repo.default_branch.as_deref().filter(|b| !b.is_empty()) {
        Some(branch) => branch.to_owned(),
        None => {
            let bytes = gitops::run_git(hub, &["-C", &dir.to_string_lossy(), "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map_err(|_| "KCI4055 the trusted default branch could not be read")?;
            String::from_utf8_lossy(&bytes).trim().to_owned()
        }
    };
    if !kiln_seams::dependencies::reference_valid(&branch) { return Err("KCI4055 the trusted default branch is invalid".into()); }
    let target = format!("refs/heads/{branch}^{{commit}}");
    let bytes = gitops::run_git(hub, &["-C", &dir.to_string_lossy(), "rev-parse", "--verify", &target], LS_REMOTE_TIMEOUT).await.map_err(|_| "KCI4055 the trusted default-branch commit is unavailable")?;
    let commit = String::from_utf8_lossy(&bytes).trim().to_owned();
    for path in [".kiln/config.yaml", ".kiln/config.yml"] {
        if let Some(bytes) = super::locks::blob(hub, &dir, None, &commit, path).await? {
            let config: RepoConfig = serde_yaml::from_slice(&bytes).map_err(|_| format!("KCI4055 {path}: trusted lock policy cannot be read"))?;
            return Ok(hub.cfg.ci.locks.stricter(config.locks()));
        }
    }
    Ok(hub.cfg.ci.locks.stricter(Default::default()))
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
    fn w29_r26_lock_policy_defaults_and_stricter_trusted_settings() -> anyhow::Result<()> {
        use kiln_ci::lock::verify::{MovedTags, Policy, Require};
        assert_eq!(RepoConfig::default().locks(), Policy::default());
        let report: RepoConfig = serde_yaml::from_str("require_lock: report\nmoved_tags: flag\n")?;
        let enforce = Policy { require: Require::Enforce, moved_tags: MovedTags::Flag };
        assert_eq!(report.locks().stricter(enforce), Policy { require: Require::Enforce, moved_tags: MovedTags::Refuse });
        assert_eq!(enforce.stricter(report.locks()), report.locks().stricter(enforce));
        assert!(serde_yaml::from_str::<RepoConfig>("require_lock: enabled\n").is_err());
        assert!(serde_yaml::from_str::<RepoConfig>("moved_tags: accept\n").is_err());
        let server: crate::config::ci::Ci = toml::from_str("[locks]\nrequire = 'report'\nmoved_tags = 'refuse'\n")?;
        assert_eq!(server.locks, Policy { require: Require::Report, moved_tags: MovedTags::Refuse });
        Ok(())
    }

    /// W29-R17: `share:` is who may use the repository's actions and reusable workflows; not
    /// shared unless it says.
    #[test]
    fn w29_r17_share_from_config() -> anyhow::Result<()> {
        let c: RepoConfig = serde_yaml::from_str("github_workflows: off\n")?;
        assert_eq!(c.share(), kiln_ci::sharing::Access::None, "not shared unless it says");
        for (yaml, access) in [("share: owner\n", kiln_ci::sharing::Access::Owner), ("share: everyone\n", kiln_ci::sharing::Access::Everyone), ("share: none\n", kiln_ci::sharing::Access::None), ("share: nobody\n", kiln_ci::sharing::Access::None)] {
            let c: RepoConfig = serde_yaml::from_str(yaml)?;
            assert_eq!(c.share(), access, "{yaml}");
        }
        Ok(())
    }

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
