//! What deploying by API keeps (docs/deploy-api.md §14.1): environments, apps, registrations and
//! releases, and the file a release writes to the release repository, `release.json` (§6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The CD provider kiln always has: its own engine (docs/deploy-api.md §8.2).
pub const KILNCD: &str = "kilncd";

/// kiln's own git server, the git host every kiln has (docs/deploy-api.md §7).
pub const KILN_HOST: &str = "kiln";

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
    /// What a release waits for before its commit. Only `concurrency` is enforced so far (§9.1).
    #[serde(default)]
    pub gates: Value,
    #[serde(default)]
    pub sync: SyncSettings,
    #[serde(default)]
    pub watch: WatchSettings,
    /// A release asked for while another is in flight: refused (`reject`) or kept waiting (`queue`).
    #[serde(default)]
    pub concurrency: Concurrency,
}

fn kilncd() -> String {
    KILNCD.to_owned()
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

fn releases() -> String {
    "releases".to_owned()
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

/// How long a release is followed, and how long it must stay synced and healthy to count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSettings {
    #[serde(default = "fifteen_minutes")]
    pub timeout_secs: u64,
    #[serde(default = "half_a_minute")]
    pub settle_secs: u64,
}

impl Default for WatchSettings {
    fn default() -> Self {
        Self { timeout_secs: fifteen_minutes(), settle_secs: half_a_minute() }
    }
}

fn fifteen_minutes() -> u64 {
    15 * 60
}

fn half_a_minute() -> u64 {
    30
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
    #[serde(default)]
    pub archived: bool,
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
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Config => "config",
            Self::Rollback => "rollback",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Waiting,
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
        matches!(self, Self::Waiting | Self::Committing | Self::Delivering)
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
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

/// `release.json`: what an environment runs, as kiln commits it (docs/deploy-api.md §6). Argo CD's
/// git files generator reads its keys as parameters, so they stay flat and few.
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
}

/// `release.json`'s schema, and the start of every release repository path kiln owns.
pub const SCHEMA: u32 = 1;
