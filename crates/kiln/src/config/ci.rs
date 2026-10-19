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
    /// `[[ci.notify]]`: the server's notification rules (docs/ci.md W19-R06), before each
    /// repository's `.kiln/notify.yaml`:
    ///
    /// ```toml
    /// [[ci.notify]]
    /// on = ["run.failed", "run.fixed"]
    /// where = "github.ref_name == 'main'"
    /// to = ["slack:#ci"]
    /// throttle = "10m"
    /// ```
    pub notify: Vec<crate::ci::notify::Rule>,
    pub scheduler: Scheduler,
    pub fairness: Fairness,
    pub insights: Insights,
    pub untrusted: Untrusted,
    /// Where requests to addresses someone gave kiln may go besides the public internet (docs/ci.md
    /// W22-AC07): webhooks, channels and integrations' checks never reach this machine, private
    /// networks or link-local addresses (a cloud's metadata service) unless one of these allows
    /// it: a host (`hooks.corp`, `hooks.corp:8443`, `*.corp`), an address, or a network
    /// (`10.0.0.0/8`).
    pub allow_private_targets: Vec<String>,
    /// `[ci.github]`: the GitHub App kiln acts through on GitHub (docs/ci.md W14-R10, W23): its
    /// id and private key, or none (the App the manifest flow made, kept in the store).
    ///
    /// ```toml
    /// [ci.github]
    /// app_id = 123456
    /// private_key_file = "/etc/kiln/github-app.pem"
    /// webhook_secret_file = "/etc/kiln/github-webhook-secret"
    /// # GitHub Enterprise Server: api_url = "https://ghe.example.com/api/v3", web_url = "https://ghe.example.com"
    /// ```
    pub github: GithubApp,
    /// `[[ci.gitlab]]`: GitLab instances kiln acts on (docs/ci.md W23-R04; docs/deploy-api.md §7),
    /// GitLab.com or self-managed, each with a token (a group's or project's access token, or a
    /// person's) with the `api` scope.
    ///
    /// ```toml
    /// [[ci.gitlab]]
    /// url = "https://gitlab.com"
    /// token_file = "/etc/kiln/gitlab-token"
    /// webhook_secret_file = "/etc/kiln/gitlab-webhook-secret"   # the webhooks' secret token
    /// groups = ["acme"]                                          # what the token covers; none: everything
    /// ```
    pub gitlab: Vec<GitlabHost>,
    /// `[ci.bitbucket]`: Bitbucket Cloud (docs/ci.md W23-R05), with an access token (a workspace's,
    /// project's or repository's), or a user and an app password or API token (an API token's user
    /// is the account's email).
    ///
    /// ```toml
    /// [ci.bitbucket]
    /// token_file = "/etc/kiln/bitbucket-token"
    /// # or: username = "kiln-bot", password_file = "/etc/kiln/bitbucket-app-password"
    /// webhook_secret_file = "/etc/kiln/bitbucket-webhook-secret"
    /// workspaces = ["acme"]
    /// ```
    pub bitbucket: Option<BitbucketHost>,
    /// `[ci.land_signing]`: the land queue signs the commits it writes (docs/ci.md W14-R07).
    ///
    /// ```toml
    /// [ci.land_signing]
    /// key = "/etc/kiln/land_ed25519"   # an SSH private key; with format = "openpgp", a key id
    /// ```
    pub land_signing: Option<LandSigning>,
    /// `kiln workflow run`'s own server (docs/ci.md W17-R06): every leg runs on its one runner,
    /// whatever names for runners its `runs-on` uses. Not a setting.
    #[serde(skip)]
    pub local: bool,
}

/// The GitHub App's settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubApp {
    pub app_id: Option<u64>,
    pub private_key_file: Option<std::path::PathBuf>,
    pub webhook_secret_file: Option<std::path::PathBuf>,
    /// GitHub's REST API.
    pub api_url: String,
    /// GitHub's web origin: repositories there are GitHub's.
    pub web_url: String,
}

impl Default for GithubApp {
    fn default() -> Self {
        GithubApp { app_id: None, private_key_file: None, webhook_secret_file: None, api_url: "https://api.github.com".into(), web_url: "https://github.com".into() }
    }
}

/// A GitLab kiln acts on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabHost {
    /// Its web origin: repositories there are this GitLab's.
    pub url: String,
    /// Its REST API (`{url}/api/v4` unless set).
    pub api_url: Option<String>,
    pub token_file: std::path::PathBuf,
    pub webhook_secret_file: Option<std::path::PathBuf>,
    /// The groups the token covers (`acme`, `acme/platform`); none: every project there.
    pub groups: Vec<String>,
}

impl Default for GitlabHost {
    fn default() -> Self {
        GitlabHost { url: "https://gitlab.com".into(), api_url: None, token_file: std::path::PathBuf::new(), webhook_secret_file: None, groups: Vec::new() }
    }
}

/// Bitbucket Cloud, as kiln acts on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BitbucketHost {
    pub web_url: String,
    pub api_url: String,
    /// An access token.
    pub token_file: Option<std::path::PathBuf>,
    /// Or a user and their app password (or API token).
    pub username: Option<String>,
    pub password_file: Option<std::path::PathBuf>,
    pub webhook_secret_file: Option<std::path::PathBuf>,
    /// The workspaces it covers; none: every one.
    pub workspaces: Vec<String>,
}

impl Default for BitbucketHost {
    fn default() -> Self {
        BitbucketHost {
            web_url: "https://bitbucket.org".into(),
            api_url: "https://api.bitbucket.org/2.0".into(),
            token_file: None,
            username: None,
            password_file: None,
            webhook_secret_file: None,
            workspaces: Vec::new(),
        }
    }
}

/// How landed commits are signed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LandSigning {
    /// An SSH private key file (`ssh`), or a key id (`openpgp`, `x509`).
    pub key: String,
    /// `ssh` (the default), `openpgp` or `x509`, as git's `gpg.format`.
    pub format: Option<String>,
}

impl Ci {
    /// Where requests to addresses someone supplied may go (`allow_private_targets`).
    pub fn reach(&self) -> crate::http::io::Reach {
        crate::http::io::Reach { allow: self.allow_private_targets.clone() }
    }
}

/// `[ci.scheduler]`: how waiting legs are ordered (docs/ci.md W05-R09, R10).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scheduler {
    /// Effective priority a waiting leg gains per minute, so nothing starves.
    pub aging_per_minute: f64,
    /// Seconds a leg is expected to take when its job has no history (W19's medians).
    pub default_cost_seconds: f64,
}

impl Default for Scheduler {
    fn default() -> Self {
        let d = kiln_ci::sched::state::Config::default();
        Scheduler { aging_per_minute: d.aging_per_minute, default_cost_seconds: d.default_cost }
    }
}

/// `[ci.fairness]`: fair share and quotas by repository name (docs/ci.md W05-R10, R11).
///
/// ```toml
/// [ci.fairness]
/// weights = { "acme/app" = 2 }   # 1 when not named
/// quotas = { "acme/big" = 10 }   # legs in progress at most
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fairness {
    pub weights: std::collections::HashMap<String, f64>,
    pub quotas: std::collections::HashMap<String, usize>,
}

/// `[ci.insights]` (docs/ci.md W19-R01): what a runner-minute costs, by pool (a runner's
/// `pool:<name>` label; `default` for the rest), for the cost columns of Insights.
///
/// ```toml
/// [ci.insights]
/// cost_per_minute = { default = 0.008, gpu = 0.12 }
/// currency = "USD"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Insights {
    pub cost_per_minute: std::collections::HashMap<String, f64>,
    pub currency: String,
}

/// `[ci.untrusted]`: runs of code from outside a repository's trust (docs/ci.md W18-R05..R08):
/// fork pull requests, other people's local runs, and what this marks.
///
/// ```toml
/// [ci.untrusted]
/// require_approval = "first-time"   # or "all-external", "none"
/// executors = ["firecracker", "shards", "docker"]
/// allow_hosts = ["registry.npmjs.org", "*.pypi.org", "files.pythonhosted.org"]
/// repos = ["acme/sandbox-*"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Untrusted {
    /// Which untrusted runs wait for a maintainer before any job starts (W18-R06).
    pub require_approval: kiln_ci::trust::Approval,
    /// The executors untrusted legs may run on, in order (W18-R07). The host only when listed.
    pub executors: Vec<String>,
    /// Docker for untrusted legs even when its engine runs as root (rootless only by default).
    pub rootful_docker: bool,
    /// Hosts (`*.example.com` for those under it, `:port` to narrow) untrusted legs may reach
    /// besides kiln's own server (W18-R08).
    pub allow_hosts: Vec<String>,
    /// Repositories (globs, `owner/name`) whose runs are all untrusted.
    pub repos: Vec<String>,
    /// People whose runs are untrusted wherever they run.
    pub actors: Vec<String>,
}

impl Default for Untrusted {
    fn default() -> Self {
        Untrusted {
            require_approval: kiln_ci::trust::Approval::FirstTime,
            executors: vec!["firecracker".into(), "shards".into(), "docker".into()],
            rootful_docker: false,
            allow_hosts: Vec::new(),
            repos: Vec::new(),
            actors: Vec::new(),
        }
    }
}

impl Untrusted {
    /// Whether an administrator marked runs of `repo` by `actor` untrusted.
    pub fn marks(&self, repo: &str, actor: &str) -> bool {
        self.repos.iter().any(|g| crate::pipeline::glob_match(&g.to_ascii_lowercase(), &repo.to_ascii_lowercase())) || self.actors.iter().any(|a| a.eq_ignore_ascii_case(actor))
    }

    /// What routing needs of it.
    pub fn route(&self) -> kiln_ci::sched::routing::Untrusted {
        kiln_ci::sched::routing::Untrusted { executors: self.executors.clone(), rootful_docker: self.rootful_docker }
    }
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
