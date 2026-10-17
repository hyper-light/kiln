//! `[ci]`: CI settings (docs/ci.md). Each workstream adds its own section here as a field of
//! [`Ci`] with its own struct below, every field defaulted, so a config without `[ci]` keeps
//! today's behaviour.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ci {
    /// The CI engine runs GitHub workflows (docs/ci.md M1). Off until the engine is complete;
    /// `.kiln/` pipelines keep running on v1 either way.
    pub enabled: bool,
    /// Whether `.github/workflows` run (docs/ci.md W02-R02): `auto` (when a commit has no
    /// `.kiln/` workflow), `on` or `off`. Repositories will be able to override it.
    pub github_workflows: kiln_ci::wf::GithubWorkflows,
    /// Route GitHub's hosted labels (`ubuntu-latest`, `macos-latest`, …) by OS and architecture
    /// (docs/ci.md W05-R21). Off by default: then such a label must be on a runner, as it must for
    /// GitHub's self-hosted runners.
    pub github_labels: bool,
    pub cache: Cache,
    pub artifacts: Artifacts,
}

/// `[ci.cache]`: GitHub's cache service (docs/ci.md W08-R06).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    /// Bytes of cache entries a repository keeps; past it, the least recently used go (GitHub's
    /// 10 GB).
    pub repo_quota_gb: u64,
    /// Entries unused this long are removed (GitHub's 7 days).
    pub max_idle_days: u64,
    /// The largest entry accepted.
    pub max_entry_gb: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Cache { repo_quota_gb: 10, max_idle_days: 7, max_entry_gb: 10 }
    }
}

/// `[ci.artifacts]`: GitHub's artifact service (docs/ci.md W08-R08).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Artifacts {
    /// How long artifacts are kept when a step asks for no `retention-days`, and the most it may
    /// ask for.
    pub retention_days: u32,
    /// The largest artifact accepted.
    pub max_size_gb: u64,
}

impl Default for Artifacts {
    fn default() -> Self {
        Artifacts { retention_days: 90, max_size_gb: 10 }
    }
}
