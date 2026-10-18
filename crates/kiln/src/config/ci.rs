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
    /// Run `.kiln/` workflows on the CI engine too (docs/ci.md W02-R06): the kiln dialect, with
    /// its typed outputs, exports, implicit checkout and the rest. v1 then runs nothing for a
    /// commit the engine took. Off by default: `.kiln/` pipelines stay on v1 until it's on.
    pub kiln_workflows: bool,
    /// Route GitHub's hosted labels (`ubuntu-latest`, `macos-latest`, …) by OS and architecture
    /// (docs/ci.md W05-R21). Off by default: then such a label must be on a runner, as it must for
    /// GitHub's self-hosted runners.
    pub github_labels: bool,
    /// `[[ci.routing]]`: the server's routing rules (docs/ci.md W05-R21), applied before any
    /// organization's, repository's or default rule, so their executor and image win:
    ///
    /// ```toml
    /// [[ci.routing]]
    /// match = { labels = ["ubuntu-latest"] }
    /// executor = ["host"]
    /// ```
    pub routing: Vec<kiln_ci::sched::routing::Rule>,
    pub cache: Cache,
    pub artifacts: Artifacts,
    pub oidc: Oidc,
    pub actions: Actions,
}

/// `[ci.actions]`: which remote actions runs may use, and what to mirror ahead (docs/ci.md
/// W07-R04). Repositories can narrow this further (`/api/ci/actions/policy?repo=`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Actions {
    /// `owner/repo[/path][@ref]` globs a run may use; any when empty.
    pub allow: Vec<String>,
    /// Globs a run may not use, whatever `allow` says.
    pub deny: Vec<String>,
    /// Refuse tags and branches: every remote action pinned by its full commit SHA (`KCI4021`).
    pub require_sha_pins: bool,
    /// Actions to mirror by `kiln actions sync` (`owner/repo@ref`), for servers that will run
    /// without reaching GitHub.
    pub prefetch: Vec<String>,
}

impl Actions {
    pub fn policy(&self) -> kiln_ci::policy::Policy {
        kiln_ci::policy::Policy { allow: self.allow.clone(), deny: self.deny.clone(), require_sha_pins: self.require_sha_pins }
    }
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

/// `[ci.oidc]`: kiln as an OpenID Connect issuer for jobs (docs/ci.md W10-R06 to W10-R10).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Oidc {
    /// The issuer URL; `<public_url>/oidc` when unset. Providers fetch its discovery document and
    /// keys from here (or from where `kiln oidc publish` put them).
    pub issuer: Option<String>,
    /// `aud` when a job asks for none; the issuer URL when unset.
    pub default_audience: Option<String>,
    /// Build `sub` from these claim keys (GitHub's `include_claim_keys`), e.g.
    /// `["repo", "context", "kiln_trust"]`; GitHub's default subject when empty.
    pub subject_claims: Vec<String>,
    /// Untrusted runs (fork pull requests) may get ID tokens too, with `kiln_trust: untrusted`.
    pub untrusted: bool,
    /// Keep the discovery document and keys written here, for a static host that serves the
    /// directory at `issuer` (W10-R12): providers that fetch from the internet read them there.
    pub publish_dir: Option<std::path::PathBuf>,
}
