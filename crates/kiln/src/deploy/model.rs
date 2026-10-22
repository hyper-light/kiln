//! What deploying by API keeps (docs/deploy-api.md §14.1): environments, apps, registrations and
//! releases, and the file a release writes to the release repository, `release.json` (§6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The CD provider kiln always has: its own engine (docs/deploy-api.md §8.2).
pub const KILNCD: &str = "kilncd";

/// kiln's own git server, the git host every kiln has (docs/deploy-api.md §7).
pub const KILN_HOST: &str = "kiln";

/// GitHub, through kiln's GitHub App (docs/deploy-api.md §7).
pub const GITHUB_HOST: &str = "github";

/// A GitLab `[[ci.gitlab]]` names, with its token (docs/deploy-api.md §7).
pub const GITLAB_HOST: &str = "gitlab";

/// Bitbucket Cloud, with `[ci.bitbucket]`'s credentials (docs/deploy-api.md §7).
pub const BITBUCKET_HOST: &str = "bitbucket";

/// Whether a release repository is on another git host (GitHub, a GitLab, Bitbucket), not kiln's own.
pub fn remote_host(host: &str) -> bool {
    matches!(host, GITHUB_HOST | GITLAB_HOST | BITBUCKET_HOST)
}

/// A stage apps are released to (docs/deploy-api.md §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub name: String,
    /// `kilncd`, or an Argo CD connection (`argocd/<name>`).
    #[serde(default = "kilncd")]
    pub provider: String,
    /// Where its releases are committed.
    #[serde(default)]
    pub release_repo: RepoRef,
    #[serde(default)]
    pub targets: Targets,
    /// What a release waits for before its commit (§9.1).
    #[serde(default, deserialize_with = "null_is_default")]
    pub gates: Gates,
    #[serde(default)]
    pub sync: SyncSettings,
    #[serde(default)]
    pub watch: WatchSettings,
    /// A release asked for while another is in flight: refused (`reject`) or kept waiting (`queue`).
    #[serde(default)]
    pub concurrency: Concurrency,
    /// What agents may do here (docs/cd-targets.md D6). Nothing, by default: a person releases.
    #[serde(default)]
    pub agents: AgentPolicy,
    /// What an app's owner teams may do here without `environments.deploy` on it (§15). Nothing,
    /// by default.
    #[serde(default, skip_serializing_if = "OwnerPolicy::is_default")]
    pub owners: OwnerPolicy,
    /// How kiln drives Argo CD here (§8.3), when it's the provider.
    #[serde(default)]
    pub argocd: ArgoSettings,
}

/// How kiln drives an Argo CD environment's Applications (docs/deploy-api.md §8.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgoSettings {
    /// kiln writes each Application itself, for an Argo CD that doesn't allow ApplicationSets.
    /// Otherwise one ApplicationSet over the release repository makes them.
    #[serde(default)]
    pub applications: bool,
}

/// What agents may do in an environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    /// Release apps here: deploy, config, rollback, enable, disable, cancel, deregister.
    #[serde(default)]
    pub deploy: bool,
    /// Answer what waits for an approval here (when gates do).
    #[serde(default)]
    pub approve: bool,
}

/// What an app's owner teams may do in an environment (docs/deploy-api.md §15).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerPolicy {
    /// Release their app here: deploy, config, rollback, enable, disable, cancel, and act on what
    /// it runs (a restart, a scale).
    #[serde(default)]
    pub release: bool,
}

impl OwnerPolicy {
    pub fn is_default(&self) -> bool {
        *self == OwnerPolicy::default()
    }
}

fn kilncd() -> String {
    KILNCD.to_owned()
}

/// A field left out, or `null`, is its default.
fn null_is_default<'de, D: serde::Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// What a release waits for before its commit (docs/deploy-api.md §9.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gates {
    /// Environments a new version runs in first: the same version, with the same artifacts,
    /// succeeded there and still runs there (txst's required environments). `staging@eu` is
    /// `staging` on the linked kiln server `eu` (§11), which signs what it says runs there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// How long it must have run in each of them (`30m`, `2h`, `1d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soak: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Approvals>,
    /// When releases may go (`allow`) or may not (`deny`), as Argo CD's sync windows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<Window>,
    /// Checks each release passes: built in (`no-snapshots`, `owners-set`, by name) or external
    /// (a URL that answers, a kiln workflow).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// Each artifact a new version deploys has kiln's verified provenance, meeting this (docs/ci.md
    /// W18-R04).
    #[serde(default, alias = "require-provenance", skip_serializing_if = "Option::is_none")]
    pub require_provenance: Option<Provenance>,
    /// Held, and why (an incident, a holiday): no release goes until it's lifted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freeze: Option<String>,
    /// `auto`: a version that lands in one of `after` is released here by itself, exactly what
    /// landed (§9.2), through this environment's other gates (its soak, its approvals);
    /// `manual`, the default: someone releases it.
    #[serde(default, skip_serializing_if = "Promote::is_manual")]
    pub promote: Promote,
    /// How a release reaches the release repository: its commit (`commit`, the default), or a pull
    /// request a person merges (`pull-request`): the merge is the approval, and a pull request
    /// closed unmerged cancels the release.
    #[serde(default, skip_serializing_if = "Via::is_commit")]
    pub via: Via,
}

/// How a release reaches the release repository (docs/deploy-api.md §4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    #[default]
    Commit,
    PullRequest,
}

impl Via {
    pub fn is_commit(&self) -> bool {
        *self == Via::Commit
    }
}

/// Whether a version moves on from the environments before by itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Promote {
    #[default]
    Manual,
    Auto,
}

impl Promote {
    pub fn is_manual(&self) -> bool {
        *self == Promote::Manual
    }
}

/// People who approve a release before it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvals {
    /// How many.
    #[serde(default = "one")]
    pub count: u32,
    /// Who may, by name; anyone who may approve releases here (`runs.approve`) when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    /// Whoever asked for the release may approve it too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub self_approve: bool,
}

fn one() -> u32 {
    1
}

/// A window: it opens at each time its schedule names, for `duration`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Releases go only while a window of these is open (five-field cron).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<String>,
    /// Releases wait while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    /// `90m`, `2h`, `1d`.
    pub duration: String,
    /// The schedule's time zone (`Europe/Berlin`); UTC when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// A check a release passes (§9.1): one built in, by its name (`no-snapshots`), or one described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Check {
    Named(String),
    Spec(CheckSpec),
}

impl<'de> Deserialize<'de> for Check {
    /// A name, or a description read as one (so a field misspelled is named, not "no variant").
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(name) => Ok(Check::Named(name)),
            spec => serde_json::from_value(spec).map(Check::Spec).map_err(serde::de::Error::custom),
        }
    }
}

impl Check {
    pub fn name(&self) -> &str {
        match self {
            Check::Named(n) => n,
            Check::Spec(s) => &s.name,
        }
    }

    /// Recorded with the release, never holding or refusing it.
    pub fn report_only(&self) -> bool {
        matches!(self, Check::Spec(s) if s.report_only)
    }
}

/// A check as described: a built-in one by its name, or an external one (a URL, or a workflow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    /// A built-in check's name, or this one's own (`smoke`).
    pub name: String,
    /// A URL kiln asks about each release (POST, the release as JSON): it answers `pass`, `fail`
    /// or `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpCheck>,
    /// A workflow of one of kiln's repositories, run for each release (`workflow_dispatch`): it
    /// passes when the run succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowCheck>,
    /// Its result is recorded with the release, and never holds or refuses it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_only: bool,
}

/// A URL that answers whether a release may go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpCheck {
    pub url: String,
    /// How long it may take to answer (`10s`, the default; at most `2m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// A workflow run for each release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCheck {
    /// One of kiln's repositories (`owner/name`).
    pub repository: String,
    /// The workflow: its path (`.kiln/smoke.yaml`) or file name.
    pub workflow: String,
    /// The branch or tag it runs at: the repository's default branch when none.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

/// The provenance an environment asks of what it deploys (docs/ci.md W18-R04): kiln's own,
/// signed, about each artifact's digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Who built it: kiln's own CI (`<public_url>/ci@v1`) when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<String>,
    /// The workflows it may come from (paths, globs); any when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    /// The branches it may be built from (globs); any when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
}

/// A gate's result on a release, with its evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: String,
    pub passed: bool,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub evidence: Value,
}

/// A person's answer to a release waiting for approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// Who, as the audit names them.
    pub by: String,
    pub at: i64,
    #[serde(default = "yes")]
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A release repository: a repository and branch on a git host, and the directory under which
/// registrations live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    #[serde(default = "kiln_host")]
    pub host: String,
    #[serde(default = "releases")]
    pub repo: String,
    #[serde(default = "main_branch")]
    pub branch: String,
    #[serde(default)]
    pub root: String,
}

fn kiln_host() -> String {
    KILN_HOST.to_owned()
}

/// The release repository's default name: a native repository of kiln's own (ci.md W14).
fn releases() -> String {
    "kiln/releases".to_owned()
}

fn main_branch() -> String {
    "main".to_owned()
}

impl Default for RepoRef {
    fn default() -> Self {
        Self { host: kiln_host(), repo: releases(), branch: main_branch(), root: String::new() }
    }
}

/// Where an environment runs things.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    /// Clusters, by the provider's names for them. None: the cluster kiln runs in.
    #[serde(default)]
    pub kubernetes: Vec<String>,
    /// Beyond Kubernetes, by provider, as `.kiln/environments.yaml` names them (docs/cd-targets.md
    /// §4): `aws-lambda: { account: aws-prod, region: us-east-1 }`. KilnCD environments only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platforms: BTreeMap<String, serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSettings {
    #[serde(default = "yes")]
    pub prune: bool,
    #[serde(default)]
    pub self_heal: bool,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self { prune: true, self_heal: false }
    }
}

fn yes() -> bool {
    true
}

/// How long a release is followed, how long it must stay synced and healthy to count, and how
/// long its provider may refuse it before it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSettings {
    #[serde(default = "fifteen_minutes")]
    pub timeout_secs: u64,
    #[serde(default = "half_a_minute")]
    pub settle_secs: u64,
    /// A provider that keeps refusing a release this long (its package doesn't render, its spec is
    /// invalid) fails it, and its commit is reverted: a passing trouble gets this long to clear.
    #[serde(default = "a_minute")]
    pub refused_secs: u64,
    /// A provider that took a release and can't be asked since is waited on this long, the
    /// release judged when it answers; past it, the release fails held (what it did is unknown).
    #[serde(default = "six_hours")]
    pub unreachable_secs: u64,
    /// A release synced and Degraded this long (Kubernetes' progress deadline passed, pods that
    /// can't start) has failed: room for a passing trouble to clear.
    #[serde(default = "a_minute")]
    pub degraded_secs: u64,
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self { timeout_secs: fifteen_minutes(), settle_secs: half_a_minute(), refused_secs: a_minute(), unreachable_secs: six_hours(), degraded_secs: a_minute() }
    }
}

fn six_hours() -> u64 {
    6 * 60 * 60
}

fn fifteen_minutes() -> u64 {
    15 * 60
}

fn half_a_minute() -> u64 {
    30
}

fn a_minute() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    #[default]
    Reject,
    Queue,
}

/// What a team owns and ships (docs/deploy-api.md §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub name: String,
    #[serde(default = "default_project")]
    pub project: String,
    /// Owner teams: the owner first, then a co-owner and an ops owner, if any.
    #[serde(default)]
    pub owners: Vec<String>,
    pub package: Package,
    /// The namespace it deploys to on Kubernetes; its name when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    /// The template it was made from (§3.2): its namespace pattern, checks and properties hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Who to reach, and where its release messages go.
    #[serde(default, skip_serializing_if = "Contacts::is_empty")]
    pub contacts: Contacts,
    /// Its source repository (§3.1), as kiln names it: `acme/web`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

/// Who to reach about an app (§3.1): on call, the chat channels its release messages go to, links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oncall: Option<String>,
    /// `#deploys`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// Dashboards, runbooks, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
}

impl Contacts {
    pub fn is_empty(&self) -> bool {
        self.oncall.is_none() && self.channels.is_empty() && self.links.is_empty()
    }
}

/// What apps are made from (docs/deploy-api.md §3.2): data, never code. txst's four app types are
/// four templates; there can be any number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The package a new app starts with; `{app}` in it is the app's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<Package>,
    /// Where its apps run on Kubernetes: `{app}`, `apps-{app}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Checks every release of its apps passes, wherever it goes (§9.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    /// Properties its apps may have, each with the values allowed (none listed: any).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Vec<String>>,
    /// The environments its apps may be registered in (none: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<String>,
    /// The source repository kiln makes for each of its apps (§3.1), unless the app names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepoTemplate>,
    /// The cluster-scoped kinds its apps may make (`ClusterRole`, as `rbac.authorization.k8s.io/ClusterRole`;
    /// a core kind by its name alone): on Argo CD its apps' AppProject allows them, and a Namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cluster_kinds: Vec<String>,
}

/// The source repository a template makes for each app: one of kiln's own, its first commit the
/// starter's files with `{{kiln.app}}`, `{{kiln.project}}`, `{{kiln.owner}}` and
/// `{{kiln.repository}}` filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoTemplate {
    /// `owner/name`, with `{app}` and `{project}` filled in.
    #[serde(default = "default_repo_name")]
    pub name: String,
    /// The repository whose files it starts with (https://, ssh://, git:// or user@host:path); an
    /// empty repository when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter: Option<String>,
    /// The starter's branch or tag; its default branch when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter_ref: Option<String>,
    /// The new repository's default branch.
    #[serde(default = "main_branch")]
    pub branch: String,
}

fn default_repo_name() -> String {
    "{project}/{app}".to_owned()
}

fn default_project() -> String {
    "default".to_owned()
}

impl App {
    pub fn namespace(&self) -> &str {
        self.namespace.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.name)
    }
}

/// How an app is deployed. `set` takes `${{ release.version }}`, `${{ release.id }}` and
/// `${{ release.artifacts.<name>.<repository|tag|digest> }}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Package {
    /// A Helm chart from an OCI (`oci://…`) or Helm repository, at the release's version.
    Helm {
        repo: String,
        chart: String,
        /// The chart's version, when it isn't the release's (`${{ release.version }}` by default).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chart_version: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        set: BTreeMap<String, String>,
    },
    /// A path in a git repository (Helm, Kustomize or plain manifests), at the release's version
    /// as a branch, tag or commit.
    Git {
        repo: String,
        #[serde(default)]
        path: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        set: BTreeMap<String, String>,
    },
    /// A platform KilnCD deploys to beyond Kubernetes (docs/cd-targets.md §3): its provider
    /// (`aws-lambda`, `aws-ecs`, `cloudflare-workers`, `vercel`) and its definition, as the
    /// provider's key in `.kiln/deploy.yaml` holds it, with `${{ release.* }}`, `${{ env.name }}`
    /// and `${{ app.name }}` filled for each release (KilnCD's `${{ git.* }}` and
    /// `${{ artifacts.* }}` left to it). The platform's own files (a task definition,
    /// `wrangler.jsonc`) come from `repo` at `path`, at the release's version. KilnCD environments
    /// only.
    Platform {
        provider: String,
        #[serde(default)]
        spec: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        path: String,
    },
}

impl Package {
    /// What a release sets (Helm's `--set`); a platform's definition holds its own settings.
    pub fn set(&self) -> &BTreeMap<String, String> {
        static NONE: BTreeMap<String, String> = BTreeMap::new();
        match self {
            Self::Helm { set, .. } | Self::Git { set, .. } => set,
            Self::Platform { .. } => &NONE,
        }
    }
}

/// An app in an environment (docs/deploy-api.md §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub app: String,
    pub environment: String,
    /// Releases only change what runs while it is: a disabled registration runs nothing.
    pub enabled: bool,
    /// The provider's automation is off (hand edits survive) until the next release.
    #[serde(default)]
    pub paused: bool,
    /// The release that runs, once one has landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<i64>,
    pub created_at: i64,
    /// Why what runs here needs a person (§8.1): a release failed and kiln didn't put back what ran
    /// before (a rollback, a revert, one asked not to be), or putting it back didn't land. The next
    /// release that lands clears it; until then nothing is promoted here by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A new version.
    Deploy,
    /// New values (`values.yaml`), the version kept.
    Config,
    /// Back to an earlier release's version, artifacts and values.
    Rollback,
    Enable,
    Disable,
    /// What ran before a release that failed, put back by kiln (§8.1): the failed one's commit
    /// reverted, and followed until it lands.
    Revert,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Config => "config",
            Self::Rollback => "rollback",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Revert => "revert",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Held by a gate, or by the release before it.
    Waiting,
    /// Held for people's approval.
    WaitingApproval,
    Committing,
    Delivering,
    Succeeded,
    Failed,
    Cancelled,
    /// It had succeeded; a rollback replaced it.
    RolledBack,
    /// It had succeeded; a later release replaced it.
    Superseded,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::WaitingApproval => "waiting_approval",
            Self::Committing => "committing",
            Self::Delivering => "delivering",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RolledBack => "rolled_back",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Waiting,
            Self::WaitingApproval,
            Self::Committing,
            Self::Delivering,
            Self::Succeeded,
            Self::Failed,
            Self::Cancelled,
            Self::RolledBack,
            Self::Superseded,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// Still moving: a registration has at most one release in flight.
    pub fn in_flight(self) -> bool {
        matches!(self, Self::Waiting | Self::WaitingApproval | Self::Committing | Self::Delivering)
    }
}

/// Where a release came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Asked for through the API (the UI, `kiln`, chat, MCP).
    #[default]
    Api,
    /// A commit kiln didn't make, found on the release repository.
    Git,
    /// kiln's own: a revert after a release failed.
    Kiln,
}

/// An image or other build output a release deploys, pinned by digest where known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub digest: String,
}

/// One numbered change to what an environment runs (docs/deploy-api.md §9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub id: i64,
    pub app: String,
    pub environment: String,
    pub kind: Kind,
    pub state: State,
    #[serde(default)]
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, Artifact>,
    /// A config release's new `values.yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<String>,
    /// The commit that holds it, once committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub requested_by: String,
    /// What it's waiting for, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The release it goes back to (a rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<i64>,
    /// The rollback that replaced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_by: Option<i64>,
    /// A disable that keeps what the app runs, rather than remove it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep: bool,
    /// What the provider last said about it.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub provider: Value,
    /// Each gate's result, with its evidence (§9.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<GateResult>,
    /// People's answers, while it waited for approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
    /// The upstream release it promotes (the same version and artifacts, §9.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<i64>,
    /// The key it was asked for with: asked for again with it, it is this release again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Secrets changed since what runs, that it delivers (§3.5): a secrets release's reason
    /// (`DB_PASSWORD v4`), or changes a waiting release took on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<String>,
    /// Failed, it isn't reverted: it is held for a person (asked so, as for a secret rotated after
    /// a leak, which a revert would deliver again).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_revert: bool,
    /// A revert: the release it puts back what ran before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverts: Option<i64>,
    /// The revert that puts back what ran before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted_by: Option<i64>,
    /// What it runs on its way in that a revert couldn't undo (a migration's hook), as its
    /// preflight found it: failed, it's held rather than reverted (§8.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<String>,
}

/// `release.json`: what an environment runs, as kiln commits it (docs/deploy-api.md §6): every
/// provider reads it, Argo CD's ApplicationSets through their git files generator, so it says all
/// a provider needs (the package at its version, what it sets, where it runs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseFile {
    pub schema: u32,
    pub project: String,
    pub app: String,
    pub environment: String,
    /// The release that wrote it (0: registered, never released).
    pub release: i64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub package: Package,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub requested_by: String,
    #[serde(default)]
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<i64>,
    /// The version of each of the app's secrets it delivers (`API_KEY@1,DB_PASSWORD@4`, never a
    /// value): those set as it was committed (§3.5).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secrets: String,
    /// Where it runs on Kubernetes: the app's namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// What it sets (Helm's `--set`): the package's `set`, filled for this release. Every provider
    /// sets these, so what runs is what this file says.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Whether it renders with Helm: a Helm package, or a path in git that holds a chart or is
    /// given values to set. Unset in files from before kiln wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helm: Option<bool>,
}

/// `release.json`'s schema, and the start of every release repository path kiln owns.
pub const SCHEMA: u32 = 1;
