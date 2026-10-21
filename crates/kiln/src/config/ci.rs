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
    /// Pools of executors kiln starts and stops (docs/ci.md W13-R09):
    ///
    /// ```toml
    /// [[ci.pools]]
    /// name = "docker-linux"
    /// executor = "docker"
    /// hosts = "executors contains docker"
    /// image = "kiln/runner-ubuntu-24.04"
    /// labels = ["ubuntu-latest", "linux", "x64"]
    /// min_idle = 2
    /// max = 32
    /// per_host_max = 16
    /// idle_timeout = "10m"
    /// ```
    pub pools: Vec<kiln_ci::sched::pools::PoolSpec>,
    /// Runners kept for matching legs in windows (docs/ci.md W05-R12); a pool's own go in its
    /// `reserve`:
    ///
    /// ```toml
    /// [[ci.reservations]]
    /// name = "nightly-bench"
    /// runners = "custom.rack == bench"
    /// for = "workflow == 'bench'"
    /// cron = "0 2 * * *"
    /// duration = "2h"
    /// ```
    pub reservations: Vec<kiln_ci::sched::reserve::ReservationSpec>,
    pub cache: Cache,
    pub artifacts: Artifacts,
    /// `[ci.store]`: the store under caches, artifacts, images and reports (docs/ci.md W08-R01),
    /// and the bucket it keeps its chunks in too (W08-R02):
    ///
    /// ```toml
    /// [ci.store.s3]
    /// endpoint = "https://s3.us-east-1.amazonaws.com"
    /// bucket = "acme-kiln-store"
    /// region = "us-east-1"
    /// credentials = { integration = "aws-store" }
    /// ```
    pub store: Store,
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
    /// `[ci.registry]`: kiln's own OCI registry at `/v2/` (docs/ci.md W12-R01).
    ///
    /// ```toml
    /// [ci.registry]
    /// listen = "0.0.0.0:5000"            # its own address too (the main origin serves it as well)
    /// anonymous_pull = ["acme/public"]   # namespaces anyone may pull
    /// max_blob = 21474836480             # bytes a blob may have (20 GiB)
    /// ```
    pub registry: Registry,
    /// `[ci.advisories]`: the advisory database the dependency graph's compare looks up the
    /// packages a change adds in (docs/ci.md W14-R11, for `actions/dependency-review-action`). None
    /// by default: the changes are listed without vulnerabilities, and the answer says so.
    ///
    /// ```toml
    /// [ci.advisories]
    /// osv = "https://api.osv.dev"   # OSV's API (it carries GitHub's advisories), sent those packages' purls
    /// ```
    pub advisories: Advisories,
    /// `[ci.attest]`: how kiln signs attestations (docs/ci.md W18-R02).
    ///
    /// ```toml
    /// [ci.attest]
    /// signing = "keyless"                       # kiln (its own key, the default), key, kms or keyless
    /// fulcio_url = "https://fulcio.example.com" # keyless: a Fulcio that trusts kiln's OIDC issuer
    /// rekor_url = "https://rekor.example.com"
    /// # key: key_file = "/etc/kiln/cosign.key", key_password_file = "/etc/kiln/cosign.password"
    /// # kms: kms = "awskms:///arn:aws:kms:eu-west-1:123456789012:key/…", integration = "aws-signing"
    /// ```
    pub attest: Attest,
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

/// `[ci.advisories]`'s settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Advisories {
    /// OSV's API (`https://api.osv.dev`, or a mirror of it).
    pub osv: Option<String>,
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

/// kiln's OCI registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Registry {
    /// An address of its own (`0.0.0.0:5000`), beside the main origin's `/v2/`.
    pub listen: Option<String>,
    /// Namespaces (`acme`, `acme/public`; `*`: all) anyone may pull from, without signing in.
    pub anonymous_pull: Vec<String>,
    /// The biggest blob, in bytes (20 GiB by default).
    pub max_blob: Option<u64>,
    /// The names images use for kiln's registry besides the public URL's host
    /// (`registry.example.com`): an attestation about such an image is attached to it (W18).
    pub hosts: Vec<String>,
    /// `[[ci.registry.mirrors]]`: pull-through caches of other registries (docs/ci.md W12-R02).
    ///
    /// ```toml
    /// [[ci.registry.mirrors]]
    /// upstream = "docker.io"            # ghcr.io, quay.io, gcr.io, us-docker.pkg.dev, public.ecr.aws, mcr.microsoft.com, registry.k8s.io, …
    /// listen = "0.0.0.0:5001"           # Docker's registry-mirrors (Docker Hub) pull here, at the root
    /// anonymous = true                  # pulls without signing in (a public upstream's)
    /// username = "kiln"                 # the upstream's credentials (Docker Hub's pull limits)
    /// password_file = "/etc/kiln/dockerhub-token"
    /// ```
    pub mirrors: Vec<Mirror>,
    /// `[[ci.registry.proxies]]`: caching proxies for package registries at
    /// `/_kiln/proxy/<name>/…` (docs/ci.md W12-R04, R05, R06).
    ///
    /// ```toml
    /// [[ci.registry.proxies]]
    /// ecosystem = "npm"                 # pypi, crates, go, maven, nuget, rubygems, http
    /// # upstream = "https://registry.npmjs.org"   (each ecosystem's public one by default; a private one's, R05)
    /// # token_file = "…", or username + password_file: the upstream's credentials
    /// ttl = 300                         # seconds an index is trusted before asking again
    /// ```
    pub proxies: Vec<Proxy>,
}

/// A package registry kiln proxies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Proxy {
    /// `npm`, `pypi`, `crates`, `go`, `maven`, `nuget`, `rubygems`, `http`.
    pub ecosystem: String,
    /// The path it's under (`/_kiln/proxy/<name>/`); the ecosystem's name by default.
    pub name: Option<String>,
    /// The upstream (its index's, for PyPI and crates.io), when not the ecosystem's public one.
    pub upstream: Option<String>,
    /// Where the upstream keeps its packages, when elsewhere (PyPI's files, crates.io's crates).
    pub files: Option<String>,
    /// `http`: the hosts it may reach; Go: checksum databases besides `sum.golang.org`.
    pub hosts: Vec<String>,
    /// Anyone may read through it, without signing in.
    pub anonymous: bool,
    pub token_file: Option<std::path::PathBuf>,
    pub username: Option<String>,
    pub password_file: Option<std::path::PathBuf>,
    /// Seconds an index is trusted (300).
    pub ttl: Option<u64>,
    /// Every job's tools use it by themselves: npm's, pip's and uv's, Go's, Maven's, RubyGems'
    /// and Bundler's, conda's settings point at it, with the job's token.
    pub jobs: bool,
}

/// A registry kiln pulls through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mirror {
    /// The upstream, as image references name it (`docker.io`, `ghcr.io`).
    pub upstream: String,
    /// Its API, when not the upstream's own (`https://<upstream>`; Docker Hub's is known).
    pub url: Option<String>,
    /// Names through kiln start with this (default: the upstream): `/v2/docker.io/library/alpine/…`.
    pub prefix: Option<String>,
    /// An address serving it at the root, for Docker's `registry-mirrors`.
    pub listen: Option<String>,
    /// Anyone may pull through it, without signing in.
    pub anonymous: bool,
    pub username: Option<String>,
    pub password_file: Option<std::path::PathBuf>,
    /// Seconds a tag is trusted before asking the upstream again (300).
    pub tag_ttl: Option<u64>,
}

impl Mirror {
    /// The names' prefix through kiln.
    pub fn prefix(&self) -> String {
        self.prefix.clone().unwrap_or_else(|| self.upstream.clone()).trim_matches('/').to_owned()
    }
}

impl Registry {
    pub fn max_blob_bytes(&self) -> u64 {
        self.max_blob.unwrap_or(20 << 30)
    }
}

/// How attestations are signed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Attest {
    /// `kiln` (its own key), `key`, `kms` or `keyless`.
    pub signing: Option<String>,
    /// `key`: cosign's key file (or a PKCS#8 or SEC1 PEM), and its password's file.
    pub key_file: Option<std::path::PathBuf>,
    pub key_password_file: Option<std::path::PathBuf>,
    /// `kms`: the key (`awskms:///<arn>`), and the `aws` integration kiln reaches it with.
    pub kms: Option<String>,
    pub integration: Option<String>,
    /// `keyless`: Fulcio and Rekor, and the audience of kiln's identity token for Fulcio.
    pub fulcio_url: Option<String>,
    pub rekor_url: Option<String>,
    pub audience: Option<String>,
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
    /// `slots` (each runner takes its `slots` legs at once) or `resources` (W05-R03): a runner
    /// offers what its facts say it has (CPUs, memory, the work volume's free disk, GPUs), times
    /// `overcommit`, and a leg takes what its job's `resources:` asks, else `default_request`.
    pub mode: String,
    pub overcommit: Overcommit,
    pub default_request: kiln_ci::sched::pools::ResourceSpec,
}

impl Scheduler {
    /// Resource mode (W05-R03).
    pub fn resources(&self) -> bool {
        self.mode.trim().eq_ignore_ascii_case("resources")
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        let d = kiln_ci::sched::state::Config::default();
        Scheduler { aging_per_minute: d.aging_per_minute, default_cost_seconds: d.default_cost, mode: "slots".into(), overcommit: Overcommit::default(), default_request: Default::default() }
    }
}

/// How far past what a runner has its legs may ask, in resource mode: CPUs 1.5 times, memory and
/// disk once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overcommit {
    pub cpu: f64,
    pub memory: f64,
    pub disk: f64,
}

impl Default for Overcommit {
    fn default() -> Self {
        let d = kiln_ci::sched::state::Config::default();
        Overcommit { cpu: d.overcommit_cpu, memory: d.overcommit_mem, disk: d.overcommit_disk }
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
    /// `[[ci.actions.sources]]`: where some owners' actions come from instead of GitHub, the
    /// first that names the owner (W07-R02).
    pub sources: Vec<ActionSource>,
}

impl Actions {
    pub fn policy(&self) -> kiln_ci::policy::Policy {
        kiln_ci::policy::Policy { allow: self.allow.clone(), deny: self.deny.clone(), require_sha_pins: self.require_sha_pins }
    }

    /// The source for `owner`'s actions, when one names it.
    pub fn source_of(&self, owner: &str) -> Option<&ActionSource> {
        let owner = owner.to_ascii_lowercase();
        self.sources.iter().find(|s| s.owners.iter().any(|g| crate::pipeline::glob_match(&g.to_ascii_lowercase(), &owner)))
    }
}

/// Another home for some owners' actions (W07-R02): a git host (`url`, with `{owner}` and
/// `{repo}` filled in), or the repositories kiln hosts itself (`native`, W14-R01).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionSource {
    /// Owner globs (`acme`, `acme-*`).
    pub owners: Vec<String>,
    /// The clone URL: `https://git.example.com/{owner}/{repo}.git`.
    pub url: Option<String>,
    /// `owner/repo` is one of kiln's native repositories.
    pub native: bool,
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

/// `[ci.store]`: the content-addressed store (docs/ci.md W08-R01, R02).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Store {
    /// A bucket every chunk is kept in too, the data directory's copies a cache of them (W08-R02).
    pub s3: Option<S3Store>,
}

/// `[ci.store.s3]`: any S3-compatible store (AWS S3, MinIO, Cloudflare R2, Ceph), addressed by
/// path (`<endpoint>/<bucket>/<key>`), its requests signed with Signature Version 4. One kiln
/// server to a bucket and prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Store {
    /// `https://s3.us-east-1.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com`,
    /// `http://minio.internal:9000`.
    pub endpoint: String,
    pub bucket: String,
    /// The region requests are signed for (R2's is `auto`; MinIO takes any).
    #[serde(default = "S3Store::default_region")]
    pub region: String,
    /// Where in the bucket: chunks are at `<prefix>chunks/<2 hex>/<2 hex>/<sha256>`.
    #[serde(default = "S3Store::default_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub credentials: S3Credentials,
    /// Gigabytes of chunks the data directory keeps as a cache (20 by default); past it, those the
    /// bucket holds go, least recently used first.
    #[serde(default = "S3Store::default_cache_gb")]
    pub cache_gb: u64,
    /// A PEM bundle of the CAs a private store's certificate is checked against, instead of the
    /// public ones.
    #[serde(default)]
    pub ca_file: Option<std::path::PathBuf>,
}

impl S3Store {
    fn default_region() -> String {
        "us-east-1".into()
    }
    fn default_prefix() -> String {
        "kiln/cas/".into()
    }
    fn default_cache_gb() -> u64 {
        20
    }
}

/// How kiln signs in to the bucket: an `aws` integration (kiln's own identity, exchanged for the
/// role's short-lived credentials, so no key is stored), or a key pair whose secret is read from
/// a file or an environment variable; with neither, the server's environment
/// (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct S3Credentials {
    pub integration: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key_file: Option<std::path::PathBuf>,
    pub secret_access_key_env: Option<String>,
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
    /// Where the hourly upkeep and each rotation publish the two documents (W10-R12): a directory
    /// for a static host, `s3://<bucket>[/<prefix>]` or `gs://<bucket>[/<prefix>]`. `issuer` is
    /// then where that is served (`https://<bucket>.s3.<region>.amazonaws.com/<prefix>`, say).
    pub publish_to: Option<String>,
    /// For `s3://`: the store's endpoint, AWS S3's in `publish_region` by default (MinIO's, R2's).
    pub publish_endpoint: Option<String>,
    /// For `s3://`: the region requests are signed for (`us-east-1` by default).
    pub publish_region: Option<String>,
    /// How kiln signs in to publish, as `[ci.store.s3]` takes it: an integration (`aws` for
    /// `s3://`, `gcp` for `gs://`), a key pair, or the server's environment. An `aws` integration
    /// can't publish the first keys, since AWS checks kiln's token against them: start with a
    /// key pair. Google Cloud takes the keys pasted into its pool, so a `gcp` one can.
    pub publish_credentials: S3Credentials,
    /// Addresses of the issuer's own (W10-R12), each serving its two documents and nothing else,
    /// rate-limited, so they can face the internet without the rest of kiln: `["0.0.0.0:8444"]`.
    /// Over TLS with `listen_cert` and `listen_key` (a certificate for the issuer's public name),
    /// else the server's own when `server.https`, else plain HTTP (a TLS proxy or tunnel in front).
    pub listen: Vec<String>,
    pub listen_cert: Option<std::path::PathBuf>,
    pub listen_key: Option<std::path::PathBuf>,
}
